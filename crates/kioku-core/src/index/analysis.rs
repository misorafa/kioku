//! Text analysis for the index (SPEC-M3.1 §2): the `ja` analyzer (lindera + IPADIC, NFKC,
//! plus the optional user dictionary layered on top), the identifier tokens of the `code`
//! field, the character bigrams of the `ja_bigram` field, and the user dictionary file.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, anyhow};
use lindera::dictionary::{Dictionary, load_dictionary, load_user_dictionary_from_csv};
use lindera::mode::Mode;
use lindera::segmenter::Segmenter;
use lindera_analysis::character_filter::CharacterFilter;
use lindera_analysis::character_filter::unicode_normalize::{
    UnicodeNormalizeCharacterFilter, UnicodeNormalizeKind,
};
use lindera_analysis::tokenizer::Tokenizer as LinderaTokenizer;
use regex::Regex;
use tantivy::tokenizer::{
    LowerCaser, PreTokenizedStream, PreTokenizedString, TextAnalyzer, Token, Tokenizer,
    WhitespaceTokenizer,
};

/// The starter `dict/user.csv` written by `kioku init` (SPEC-M3.1 §2).
pub const STARTER_USER_DICT: &str = "\
# kioku user dictionary / ユーザー辞書
#
# One word per line: surface,cost,part_of_speech,reading[,synonym_of]
# 1 行 1 語: 表層形,コスト,品詞,読み[,同義語の代表形]
#
# - cost: lower wins against the built-in IPADIC split; -10000 always wins. Leave it
#   empty for that default. / 小さいほど優先（-10000 で常に 1 語として扱う。空欄でも同じ）
# - synonym_of (kioku extension): the word is also indexed and searched as this word,
#   so a query for either finds both. / 指定すると、その語としても索引・検索される
# - A word never hides its parts: 引き継ぎ書 is still found by 引き継ぎ.
#   辞書の語を登録しても、その部分（引き継ぎ書 の 引き継ぎ）でも検索できる
# - Lines starting with # are comments. After editing, run `kioku reindex`
#   (`kioku doctor` warns while the index is older than this file).
#   # で始まる行はコメント。編集したら `kioku reindex` を実行する
#   （索引がこのファイルより古い間は `kioku doctor` が警告する）
#
引き継ぎ書,-10000,名詞,ヒキツギショ
引継書,-10000,名詞,ヒキツギショ,引き継ぎ書
引継,-10000,名詞,ヒキツギ,引き継ぎ
引継ぎ,-10000,名詞,ヒキツギ,引き継ぎ
レーン,-10000,名詞,レーン
セッション,-10000,名詞,セッション
観測,-10000,名詞,カンソク
索引,-10000,名詞,サクイン
プロジェクト別名,-10000,名詞,プロジェクトベツメイ
";

/// Name of the whitespace analyzer used for the pre-tokenized `code` / `ja_bigram` fields.
pub const WS_TOKENIZER: &str = "kioku_ws";

/// One word of the user dictionary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserWord {
    /// Surface form (NFKC, as the analyzer sees it).
    pub surface: String,
    /// Word cost; `None` = the dictionary's default (always wins).
    pub cost: Option<i16>,
    /// Part of speech (`名詞` when empty).
    pub pos: String,
    /// Reading (katakana), may be empty.
    pub reading: String,
    /// The word this one is also indexed and searched as (kioku extension).
    pub synonym_of: Option<String>,
}

/// Parses `dict/user.csv`: `surface,cost,pos,reading[,synonym_of]` (or lindera's simple
/// `surface,pos,reading`), `#` comments and blank lines skipped. Returns the words and one
/// message per rejected line.
pub fn parse_user_dict(text: &str) -> (Vec<UserWord>, Vec<String>) {
    let mut words = Vec::new();
    let mut errors = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split(',').map(str::trim).collect();
        let surface = nfkc(cols[0]);
        if surface.is_empty() || surface.contains('"') || surface.chars().any(char::is_whitespace) {
            errors.push(format!("line {}: invalid surface", i + 1));
            continue;
        }
        // `surface,cost,…` when the second column is a number (or empty), else lindera's
        // simple `surface,pos,reading`.
        let (cost, rest): (Option<i16>, &[&str]) = match cols.get(1) {
            Some(&"") => (None, &cols[2..]),
            Some(c) => match c.parse::<i16>() {
                Ok(v) => (Some(v), &cols[2..]),
                Err(_) if c.chars().any(|ch| ch.is_ascii_digit()) => {
                    errors.push(format!("line {}: invalid cost {c}", i + 1));
                    continue;
                }
                Err(_) => (None, &cols[1..]),
            },
            None => (None, &cols[1..]),
        };
        let col = |n: usize| rest.get(n).map(|s| s.to_string()).unwrap_or_default();
        let pos = Some(col(0)).filter(|p| !p.is_empty());
        let synonym_of =
            Some(nfkc(&col(2))).filter(|s| !s.is_empty() && *s != surface && !s.contains('"'));
        words.push(UserWord {
            surface,
            cost,
            pos: pos.unwrap_or_else(|| "名詞".to_string()),
            reading: col(1),
            synonym_of,
        });
    }
    (words, errors)
}

/// NFKC through lindera's character filter (what the `ja` analyzer applies).
pub fn nfkc(text: &str) -> String {
    let mut s = text.to_string();
    let filter = UnicodeNormalizeCharacterFilter::new(UnicodeNormalizeKind::NFKC);
    if filter.apply(&mut s).is_err() {
        return text.to_string();
    }
    s
}

/// The embedded IPADIC dictionary (loaded once per process).
fn ipadic() -> anyhow::Result<Dictionary> {
    static DICT: OnceLock<Result<Dictionary, String>> = OnceLock::new();
    DICT.get_or_init(|| load_dictionary("embedded://ipadic").map_err(|e| e.to_string()))
        .clone()
        .map_err(|e| anyhow!("loading IPADIC dictionary: {e}"))
}

fn lindera_tokenizer(segmenter: Segmenter) -> LinderaTokenizer {
    let mut tokenizer = LinderaTokenizer::new(segmenter);
    tokenizer.append_character_filter(
        UnicodeNormalizeCharacterFilter::new(UnicodeNormalizeKind::NFKC).into(),
    );
    tokenizer
}

/// The user dictionary layer of the `ja` analyzer.
struct UserLayer {
    tokenizer: LinderaTokenizer,
    /// Lowercased surface → lowercased word it is also indexed as.
    synonyms: HashMap<String, String>,
}

/// The `ja` tokenizer: IPADIC tokens (NFKC, offsets into the original text) plus, with a
/// user dictionary, the dictionary's words (at the position of the first IPADIC token they
/// overlap, so a word never hides its parts) and their synonyms.
#[derive(Clone)]
pub struct JaTokenizer {
    plain: Arc<LinderaTokenizer>,
    user: Option<Arc<UserLayer>>,
}

impl JaTokenizer {
    /// IPADIC only.
    pub fn plain() -> anyhow::Result<JaTokenizer> {
        static PLAIN: OnceLock<Result<Arc<LinderaTokenizer>, String>> = OnceLock::new();
        let plain = PLAIN
            .get_or_init(|| {
                ipadic()
                    .map(|d| Arc::new(lindera_tokenizer(Segmenter::new(Mode::Normal, d, None))))
                    .map_err(|e| format!("{e:#}"))
            })
            .clone()
            .map_err(|e| anyhow!(e))?;
        Ok(JaTokenizer { plain, user: None })
    }

    /// IPADIC plus `words` (none = [`JaTokenizer::plain`]). `scratch` is where the lindera
    /// CSV generated from the words is written.
    pub fn with_words(words: &[UserWord], scratch: &Path) -> anyhow::Result<JaTokenizer> {
        let mut tok = JaTokenizer::plain()?;
        if words.is_empty() {
            return Ok(tok);
        }
        let dictionary = ipadic()?;
        let meta = dictionary.metadata.clone();
        // IPADIC's full row: surface, left id, right id, cost, 9 detail fields.
        let mut csv = String::new();
        for w in words {
            csv.push_str(&format!(
                "{s},{l},{r},{c},{pos},*,*,*,*,*,{base},{read},{read}\n",
                s = w.surface,
                l = meta.default_left_context_id,
                r = meta.default_right_context_id,
                c = w.cost.unwrap_or(meta.default_word_cost),
                pos = w.pos.replace(',', " "),
                base = w.surface,
                read = if w.reading.is_empty() {
                    "*".to_string()
                } else {
                    w.reading.replace(',', " ")
                },
            ));
        }
        if let Some(dir) = scratch.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(scratch, csv).with_context(|| format!("writing {}", scratch.display()))?;
        let user = load_user_dictionary_from_csv(&meta, scratch)
            .map_err(|e| anyhow!("building the user dictionary: {e}"))?;
        let segmenter = Segmenter::new(Mode::Normal, dictionary, Some(user));
        let synonyms = words
            .iter()
            .filter_map(|w| {
                w.synonym_of
                    .as_ref()
                    .map(|s| (w.surface.to_lowercase(), s.to_lowercase()))
            })
            .collect();
        tok.user = Some(Arc::new(UserLayer {
            tokenizer: lindera_tokenizer(segmenter),
            synonyms,
        }));
        Ok(tok)
    }

    /// True when a user dictionary is layered on top.
    pub fn has_user_dict(&self) -> bool {
        self.user.is_some()
    }

    /// The tokens of `text`, ordered by position.
    pub fn tokens(&self, text: &str) -> Vec<Token> {
        let spans = |t: &LinderaTokenizer| -> Vec<(usize, usize, String)> {
            match t.tokenize(text) {
                Ok(tokens) => tokens
                    .into_iter()
                    .map(|t| (t.byte_start, t.byte_end, t.surface.to_string()))
                    .collect(),
                Err(e) => {
                    tracing::warn!(error = %e, "tokenizing failed; text left unindexed");
                    Vec::new()
                }
            }
        };
        let base = spans(&self.plain);
        let mut out: Vec<Token> = base
            .iter()
            .enumerate()
            .map(|(i, (from, to, text))| Token {
                offset_from: *from,
                offset_to: *to,
                position: i,
                text: text.clone(),
                position_length: 1,
            })
            .collect();
        let Some(user) = &self.user else {
            return out;
        };
        let known: HashSet<(usize, usize, String)> = base.iter().cloned().collect();
        let position_at = |from: usize| {
            base.iter()
                .position(|(_, to, _)| *to > from)
                .unwrap_or(base.len().saturating_sub(1))
        };
        for (from, to, text) in spans(&user.tokenizer) {
            if !known.contains(&(from, to, text.clone())) {
                out.push(Token {
                    offset_from: from,
                    offset_to: to,
                    position: position_at(from),
                    text,
                    position_length: 1,
                });
            }
        }
        let synonyms: Vec<Token> = out
            .iter()
            .filter_map(|t| {
                user.synonyms.get(&t.text.to_lowercase()).map(|s| Token {
                    text: s.clone(),
                    ..t.clone()
                })
            })
            .collect();
        out.extend(synonyms);
        out.sort_by_key(|t| (t.position, t.offset_from, t.offset_to));
        out.dedup_by(|a, b| {
            a.position == b.position && a.text == b.text && a.offset_from == b.offset_from
        });
        out
    }
}

impl Tokenizer for JaTokenizer {
    type TokenStream<'a> = PreTokenizedStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> PreTokenizedStream {
        PreTokenizedString {
            text: String::new(),
            tokens: self.tokens(text),
        }
        .into()
    }
}

/// The `ja` analyzer over `tokenizer`: its tokens, lowercased.
pub fn ja_analyzer_with(tokenizer: JaTokenizer) -> TextAnalyzer {
    TextAnalyzer::builder(tokenizer).filter(LowerCaser).build()
}

/// The whitespace analyzer of the pre-tokenized fields.
pub fn ws_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(WhitespaceTokenizer::default()).build()
}

/// Loads `dict` (when it exists) into a `ja` tokenizer; a missing file means IPADIC only,
/// an unreadable or broken one is logged and ignored (search must keep working).
pub fn load_ja_tokenizer(dict: Option<&Path>, scratch: &Path) -> anyhow::Result<JaTokenizer> {
    let Some(dict) = dict else {
        return JaTokenizer::plain();
    };
    let text = match std::fs::read_to_string(dict) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return JaTokenizer::plain(),
        Err(e) => {
            tracing::warn!(file = %dict.display(), error = %e, "user dictionary unreadable; ignored");
            return JaTokenizer::plain();
        }
    };
    let (words, errors) = parse_user_dict(&text);
    for e in &errors {
        tracing::warn!(file = %dict.display(), "user dictionary {e}; line skipped");
    }
    match JaTokenizer::with_words(&words, scratch) {
        Ok(t) => Ok(t),
        Err(e) => {
            tracing::warn!(file = %dict.display(), error = format!("{e:#}"), "user dictionary ignored");
            JaTokenizer::plain()
        }
    }
}

/// Identifier tokens for the `code` field (SPEC-M3.1 §2): every run like
/// `crates/kioku-core/src/store.rs`, `Store::open` or `kioku_handoff_write` (after NFKC)
/// yields the whole run, its `/` and `::` segments, its words and their `snake_case` /
/// `camelCase` parts, lowercased, without repeats within the run.
pub fn code_tokens(text: &str) -> Vec<String> {
    identifier_runs(text).into_iter().flatten().collect()
}

/// The lowercased parts (not the whole) of the compound identifiers in `query` (runs that
/// split into more than one token, like `write_lock` or `Store::open`). The `ja` clauses of these parts are
/// left to the `code` field, so a page that merely uses the words `write` and `lock` does not
/// outrank the one naming `write_lock` (SPEC-M3.1 §5 note).
pub fn identifier_parts(query: &str) -> HashSet<String> {
    identifier_runs(query)
        .into_iter()
        .filter(|run| run.len() > 1)
        // The whole run stays a `ja` term too (`WireGuard` is one word for lindera).
        .flat_map(|run| run.into_iter().skip(1))
        .collect()
}

/// [`code_tokens`] grouped per identifier run (the whole run first).
pub fn identifier_runs(text: &str) -> Vec<Vec<String>> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"[A-Za-z0-9_]+(?:(?:::|[/.\-])[A-Za-z0-9_]+)*").expect("valid identifier regex")
    });
    let text = nfkc(text);
    let mut out = Vec::new();
    for m in re.find_iter(&text) {
        let whole = m.as_str();
        let mut toks: Vec<String> = Vec::new();
        let mut push = |t: &str| {
            let t = t.to_lowercase();
            if !t.is_empty() && !toks.contains(&t) {
                toks.push(t);
            }
        };
        push(whole);
        for seg in whole.split('/').flat_map(|s| s.split("::")) {
            push(seg);
        }
        for word in whole.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
            push(word);
            for part in word.split('_') {
                push(part);
                for p in camel_parts(part) {
                    push(&p);
                }
            }
        }
        out.push(toks);
    }
    out
}

/// `SearchIndex` → [`"search", "index"`]; `HTTPServer` → [`"http", "server"`].
fn camel_parts(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut parts = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        let boundary = i > 0
            && c.is_ascii_uppercase()
            && (chars[i - 1].is_ascii_lowercase()
                || chars[i - 1].is_ascii_digit()
                || (chars[i - 1].is_ascii_uppercase()
                    && chars.get(i + 1).is_some_and(|n| n.is_ascii_lowercase())));
        if boundary && !cur.is_empty() {
            parts.push(std::mem::take(&mut cur));
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

fn is_kanji(c: char) -> bool {
    matches!(c, '\u{4e00}'..='\u{9fff}' | '\u{3400}'..='\u{4dbf}' | '々' | '〆' | 'ヶ')
}

fn is_hiragana(c: char) -> bool {
    ('\u{3041}'..='\u{309f}').contains(&c)
}

/// Runs of word characters (letters, digits, kana, kanji) of the NFKC-lowercased text.
fn word_runs(text: &str) -> Vec<Vec<char>> {
    let text = nfkc(text).to_lowercase();
    let mut runs = Vec::new();
    let mut cur = Vec::new();
    for c in text.chars() {
        if c.is_alphanumeric() || c == 'ー' {
            cur.push(c);
        } else if !cur.is_empty() {
            runs.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        runs.push(cur);
    }
    runs
}

fn bigrams_of(chars: &[char], out: &mut Vec<String>) {
    for w in chars.windows(2) {
        out.push(w.iter().collect());
    }
}

/// Character bigrams of `text` (NFKC, lowercased, never across spaces or punctuation) for
/// the `ja_bigram` field, plus the bigrams of each kanji "skeleton" — kanji joined across
/// okurigana of at most two hiragana — so 引き継ぎ書 also yields 引継 and 継書.
pub fn bigram_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut skeleton_grams = Vec::new();
    for run in word_runs(text) {
        bigrams_of(&run, &mut out);
        let mut skel: Vec<char> = Vec::new();
        let mut i = 0;
        let flush = |skel: &mut Vec<char>, grams: &mut Vec<String>| {
            if skel.len() >= 2 {
                bigrams_of(skel, grams);
            }
            skel.clear();
        };
        while i < run.len() {
            let c = run[i];
            if is_kanji(c) {
                skel.push(c);
                i += 1;
                continue;
            }
            if is_hiragana(c) && !skel.is_empty() {
                let mut j = i;
                while j < run.len() && is_hiragana(run[j]) {
                    j += 1;
                }
                if j - i <= 2 && j < run.len() && is_kanji(run[j]) {
                    i = j;
                    continue;
                }
            }
            flush(&mut skel, &mut skeleton_grams);
            i += 1;
        }
        flush(&mut skel, &mut skeleton_grams);
    }
    let plain: HashSet<String> = out.iter().cloned().collect();
    let mut extra: Vec<String> = skeleton_grams
        .into_iter()
        .filter(|g| !plain.contains(g))
        .collect();
    extra.dedup();
    out.extend(extra);
    out
}

/// The plain bigrams of a query (no skeleton), without repeats, in order.
pub fn query_bigrams(query: &str) -> Vec<String> {
    let mut grams = Vec::new();
    for run in word_runs(query) {
        bigrams_of(&run, &mut grams);
    }
    let mut seen = HashSet::new();
    grams.retain(|g| seen.insert(g.clone()));
    grams
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(t: &JaTokenizer, s: &str) -> Vec<String> {
        t.tokens(s).into_iter().map(|t| t.text).collect()
    }

    #[test]
    fn code_tokens_split_identifiers_and_keep_the_whole() {
        let t = code_tokens("kioku_handoff_write と Store::open、src/index.rs の write_lock");
        for want in [
            "kioku_handoff_write",
            "kioku",
            "handoff",
            "write",
            "store::open",
            "store",
            "open",
            "src/index.rs",
            "index.rs",
            "index",
            "rs",
            "write_lock",
            "lock",
        ] {
            assert!(t.contains(&want.to_string()), "{want}: {t:?}");
        }
        let t = code_tokens("SearchIndex と HTTPServer、全角 ｗｒｉｔｅ＿ｌｏｃｋ");
        for want in [
            "searchindex",
            "search",
            "index",
            "http",
            "server",
            "write_lock",
        ] {
            assert!(t.contains(&want.to_string()), "{want}: {t:?}");
        }
        assert!(code_tokens("日本語だけの文").is_empty());
    }

    #[test]
    fn bigrams_include_the_kanji_skeleton() {
        let g = bigram_tokens("引き継ぎ書を毎回作る");
        for want in ["引き", "き継", "継ぎ", "ぎ書", "引継", "継書", "毎回"] {
            assert!(g.contains(&want.to_string()), "{want}: {g:?}");
        }
        // no bigram across punctuation or spaces; NFKC folds half-width kana
        let g = bigram_tokens("ｱﾌﾟﾘ、 テスト");
        assert!(g.contains(&"アプ".to_string()), "{g:?}");
        assert!(!g.iter().any(|x| x.contains('、') || x.contains(' ')));
        assert_eq!(query_bigrams("引継"), vec!["引継"]);
        assert!(query_bigrams("鯖").is_empty());
    }

    #[test]
    fn user_dictionary_parsing() {
        let (words, errors) = parse_user_dict(
            "# comment\n\n引き継ぎ書,-10000,名詞,ヒキツギショ\n引継,,名詞,ヒキツギ,引き継ぎ\nレーン,名詞,レーン\nbad word,1,名詞,x\n観測,abc1,名詞,x\n",
        );
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert_eq!(words.len(), 3);
        assert_eq!(words[0].cost, Some(-10000));
        assert_eq!(words[1].cost, None);
        assert_eq!(words[1].synonym_of.as_deref(), Some("引き継ぎ"));
        assert_eq!(words[2].pos, "名詞");
        assert_eq!(words[2].reading, "レーン");
        let (starter, errors) = parse_user_dict(STARTER_USER_DICT);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(starter.iter().any(|w| w.surface == "プロジェクト別名"));
    }

    #[test]
    fn user_words_are_added_without_hiding_their_parts() {
        let tmp = tempfile::tempdir().unwrap();
        let (words, _) = parse_user_dict(STARTER_USER_DICT);
        let t = JaTokenizer::with_words(&words, &tmp.path().join("u.csv")).unwrap();
        assert!(t.has_user_dict());
        let toks = texts(&t, "引き継ぎ書を書いた");
        assert!(toks.contains(&"引き継ぎ書".to_string()), "{toks:?}");
        assert!(toks.contains(&"引き継ぎ".to_string()), "{toks:?}");
        let toks = texts(&t, "プロジェクト別名を登録");
        assert!(toks.contains(&"プロジェクト別名".to_string()), "{toks:?}");
        assert!(toks.contains(&"別名".to_string()), "{toks:?}");
        // synonyms are emitted at the same position
        let tokens = t.tokens("引継の手順");
        let syn = tokens
            .iter()
            .find(|t| t.text == "引き継ぎ")
            .expect("synonym");
        let orig = tokens.iter().find(|t| t.text == "引継").expect("surface");
        assert_eq!(syn.position, orig.position);
        // positions never go backwards
        assert!(tokens.windows(2).all(|w| w[0].position <= w[1].position));
        // without a dictionary nothing changes
        let plain = JaTokenizer::plain().unwrap();
        assert_eq!(
            texts(&plain, "引き継ぎ書を書いた"),
            ["引き継ぎ", "書", "を", "書い", "た"]
        );
    }
}
