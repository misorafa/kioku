#!/bin/sh
D=$1; P=$2; HOME_DIR=$3
export HOME=$HOME_DIR PATH=$HOME/.cargo/bin:$PATH
TOK=$(awk -F'"' '/^\[client\]/{c=1} c&&/^auth_token/{print $2; exit}' "$HOME/.kioku/config.toml"); [ -z "$TOK" ] && TOK=$(awk -F'"' '/^auth_token/{print $2; exit}' "$HOME/.kioku/config.toml")
U=http://127.0.0.1:47391/api/v1
post() { curl -s -m10 -X POST "$U/$1" -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json' --data-binary "$2" >/dev/null; }
put() { curl -s -m10 -X PUT "$U/$1" -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json' --data-binary "$2" >/dev/null; }
start() { post sessions/start "{\"session_id\":\"$1\",\"agent\":\"$2\",\"cwd\":\"$D/repo-en\",\"source\":\"startup\",\"machine\":\"$3\",\"project\":{\"id\":\"$P\",\"name\":\"demo-shop\",\"root\":\"$D/repo-en\",\"remote\":\"https://github.com/misorafa/demo-shop.git\"}}"; }
obs() { post observations "{\"session_id\":\"$1\",\"kind\":\"$2\",\"payload\":$3}"; }
start e1 claude-code macbook
obs e1 prompt '{"prompt":"Fix the checkout form validation"}'
obs e1 tool_use '{"tool_name":"Edit","tool_input":{"file_path":"src/checkout/form.ts"}}'
obs e1 tool_use '{"tool_name":"Bash","tool_input":{"command":"npm test -- checkout"}}'
obs e1 tool_use '{"tool_name":"Bash","tool_input":{"command":"git commit -am \"fix(checkout): validate email before card\""}}'
post handoffs "{\"project\":\"$P\",\"session\":\"e1\",\"summary\":\"Checkout now validates the email before the card number; added tests for the form\",\"next_steps\":[\"Apply the same validation to the address step\",\"Translate the error messages\"],\"open_questions\":[\"Normalize email case on the server or the client?\"],\"decisions\":[\"Validate before serialization\",\"Errors are returned as i18n keys\"],\"gotchas\":[\"npm test needs the mock payment server running\"]}"
post sessions/e1/finalize '{"reason":"demo"}'
put pages "{\"project\":\"$P\",\"title\":\"Working rules\",\"slug\":\"rules\",\"content\":\"- never push to main directly\\n- run npm test before a PR\\n- keep PRs small\",\"tags\":[\"pinned\"]}"
put pages "{\"project\":\"$P\",\"title\":\"Checkout design notes\",\"slug\":\"checkout-design\",\"content\":\"Sessions live in the DB, not in JWTs, so they can be revoked instantly.\\nValidation order: email, then card, then address.\",\"tags\":[\"design\"]}"
start e2 codex win-pc
obs e2 prompt '{"prompt":"Apply the same validation to the address step"}'
obs e2 tool_use '{"tool_name":"Edit","tool_input":{"file_path":"src/checkout/address.ts"}}'
post handoffs "{\"project\":\"$P\",\"session\":\"e2\",\"summary\":\"Address step validates the email too; listed every i18n key in docs/i18n.md\",\"next_steps\":[\"Fill in the Japanese translations\",\"Use the same order on the 2FA screen\"],\"open_questions\":[],\"decisions\":[\"i18n keys are checkout.error.<name>\"],\"verified\":[\"npm test passes end to end\"]}"
post sessions/e2/finalize '{"reason":"demo"}'
echo seeded-en
