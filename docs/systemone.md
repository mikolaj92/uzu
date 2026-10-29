# Typed decisions with SystemOne

SystemOne scores labels with the loaded chat model. It does not generate an answer token.
Chat and decisions share one copy of the weights and one inference gate.
Each question uses a fresh decoder state. Decisions do not change chat history or its random seed.

## Start the server

Use a native Uzu model that already works with the CLI. Arbitrary Hugging Face weights are not interchangeable.
The model needs a Hanashi encoding with an answer boundary that can be verified exactly.
Reasoning must be absent or support explicit disabling.

Start the server with a local model directory:

```sh
cargo +nightly run -p cli --bin cli -- server --model /absolute/path/to/model --host 127.0.0.1 --port 8000
```

Read the exact model ID from `GET /v1/models`.
Send JSON to `POST /v1/systemone`. The unversioned SystemOne route is not exposed.
There is no `/v1/decisions` alias.

```json
{"state":{"ticket":"I was charged twice. Please refund the duplicate today."},"questions":{"category":{"type":"choice","instructions":"Classify this ticket.","criteria":{"billing":"Payments and refunds","technical":"Technical issues","other":null}},"urgent":{"type":"noul","instructions":"Does the customer request action today?"},"priority":{"type":"score","instructions":null,"criteria":[null,"This week","Today"]}}}
```

Question names only associate answers with questions. They are not instructions.
Choice option order determines proxies A–Z. Score levels use proxies 0–9.
The model must encode each proxy as one distinct token after the complete prompt.
Unsupported codecs and tokenizations return 422 instead of falling back to generation.

## Use the official TypeSafe SDK

The smoke script uses `@typesafe-ai/sdk@0.7.0` without adding a workspace dependency.
Run these commands from the repository root. Keep the temporary directory to inspect its package lock.

```sh
smoke_dir=$(mktemp -d)
npm install --prefix "$smoke_dir" --ignore-scripts --no-audit --no-fund @typesafe-ai/sdk@0.7.0
cp scripts/systemone-smoke.mjs "$smoke_dir/systemone-smoke.mjs"
UZU_URL=http://127.0.0.1:8000 node "$smoke_dir/systemone-smoke.mjs"
```

Use the server root as `baseURL`, without `/v1`. The SDK adds `/v1/systemone`.
Set `compression: false`; this endpoint rejects compressed request bodies.
The script sets a 35-second client timeout and disables retries so busy responses remain visible.
The dummy `apiKey: "local"` satisfies SDK validation. It does not secure the server.

## Interpret the result

- `noul` is the probability of true, conditional on the yes/no labels. It is a number, not a boolean.
- `choice` returns the first maximum in the supplied option order.
- `score` is the expected zero-based level index. Three levels produce a value between 0 and 2.
- `probabilities` are normalized over the labels at temperature 1.
- `confidence` measures concentration. It is not calibrated factual accuracy.
- `usage.input_tokens` sums the actual prompt length for every question, including templates and instructions.
- `usage.output_tokens` is zero. There is no sampling, decoding loop, or speculative drafting.

Choice confidence is `(max(p) - 1/n) / (1 - 1/n)`. A single choice has confidence 1.
Score confidence is `max(0, 1 - D/U)`, where `D = sum(p[i] * abs(i - m))`.
Here `m` is the first maximum and `U = sum(abs(i - (n-1)/2))/n`.
For probabilities 0.1, 0.2, 0.7, score is 1.6 and confidence is 0.4.

## Supported profile and limits

State accepts text, objects, or arrays, including nested JSON values. Top-level null, numbers, and booleans are invalid.
Descriptions and instructions accept the same content, or null to mean absent.
A noul question needs non-null instructions or at least one non-null true/false criterion.
Empty strings are not null. Null score level descriptions retain their positions.

| Limit | Value |
| --- | --- |
| Request body | 1 MiB |
| Questions | 1–32 |
| Choice options | 1–26 distinct nonempty names |
| Score levels | 2–10 |
| Aggregate prepared prompt tokens | 65,536 |
| HTTP/1 header parsing, body, and inference deadline | One 30-second budget |

Each prompt must also fit the loaded model context limit. No generated-token reservation is added.
Images, nonempty assets, box questions, streaming, and compressed bodies are unsupported.
Empty or null assets are accepted. Unknown fields and duplicate JSON keys are rejected.
The optional model field must match the exact loaded ID.

## Errors and cancellation

Responses are buffered. Any question failure returns one error status, not partial answers or a committed 200.
Busy decisions return 429 with `Retry-After: 1`. Chat and decisions cannot run on the model concurrently.
Chat takes the inference gate before its session lock.

SystemOne requires HTTP/1.x. HTTP/2 requests to this endpoint return 505 with `unsupported_http_version`.
Other HTTP/2 routes remain available.

One absolute 30-second budget starts with Hyper's HTTP/1 header-parse attempt.
The same deadline covers headers, Rocket's initial body peek, body reading, and inference.
Headers that remain incomplete can cause a connection close before a route is known.
A stalled SystemOne body or expired inference returns JSON 504 and closes that HTTP/1 connection.
The deadline comes from a trusted transport extension, never from an HTTP header.

This is not a TLS-handshake, physical packet-arrival, pipelined queue-age, or client response-read limit.
Existing chat response streams do not receive this SystemOne lifetime limit.
The locked Hyper 0.14.32 and Rocket 0.5.1 copies carry a small local patch; see [provenance](../vendor/README.md).

A deadline returns 504 and requests cancellation between questions.
An active GPU score finishes before its worker releases the gate, even after the response returns.
A later request can therefore still receive 429 after a timeout.
Disconnect detection is best effort with buffered HTTP; immediate cancellation is not guaranteed.
Native scoring blocks through GPU completion, so Rust async servers must use a blocking worker.
The initial scorer is unavailable on WebAssembly.

| Status | Error code |
| --- | --- |
| 400 | invalid_request, duplicate_key, unknown_field, model_not_found, unsupported_feature |
| 413 | request_too_large, work_budget_exceeded |
| 415 | unsupported_media_type, unsupported_content_encoding |
| 422 | context_length_exceeded, unsupported_label_tokenization, unsupported_prompt_codec |
| 429 | model_busy |
| 501 | native_scoring_unavailable |
| 504 | deadline_exceeded |
| 500 | inference_error, invalid_model_output |

The server retains its existing security model. Bind to localhost; do not expose it publicly without separate protection.
JSON framing treats state as data but does not guarantee resistance to prompt injection.

## Verification

```sh
cargo +nightly test -p cli systemone
cargo +nightly test -p nagare decision
cargo +nightly test -p uzu-engine score
```

### Required real-model acceptance

Use Apple Silicon with Metal, a working nightly Rust/Metal compiler, Python 3, and Node 22 or newer.
The acceptance command never downloads a missing model and fails if its pinned files are absent or different.
Provision the public fixture explicitly, then run the same command as CI:

```sh
python3 scripts/systemone-acceptance/fixture.py --model /absolute/path/LFM2.5-350M-L
sh scripts/verify-systemone.sh --model /absolute/path/LFM2.5-350M-L --backend metal --output /absolute/path/new-evidence-directory
```

The fixture manifest pins revision `9825a0acca9efaa1c7e51fe5032a527aedace312`, weights, tokenizer, config, Hanashi encoding and the upstream license.
The weights are about 367 MB and use the LFM Open License v1.0, not Apache. Review that license before use.
No weights or installed SDK dependencies belong in Git.

One run validates the fixture, explicitly executes the Metal parity test, runs CLI/Nagare tests, builds the CLI, and starts its own loopback server.
It checks all three SystemOne types, all contextual answer proxies, A-B-A isolation, a strict greedy chat continuation around scoring,
a shared header/body deadline, timeout recovery to 200, and the unchanged smoke script with official SDK 0.7.0.
It records any observed 429 with Retry-After without assuming a fixed GPU duration. The GPU phase is not independently instrumented.
The owned server is stopped on success or failure. Logs and an atomic result.json identify the binary and final outcome.
A missing or filtered native test cannot produce PASS. The `SystemOne Liquid (Metal)` CI job runs this command; GitHub branch protection is separate configuration.

The native fixture has ten ShortConv and six attention layers, with no KV sharing.
The independent reference reads the same quantized weights with 512-token batches, compared with the scorer’s 1024-token batches.
The numerical tolerance is less than 0.1 in log probability and log label mass; logs report actual errors.
This checks scoring and lifecycle correctness, not calibration, classification accuracy, throughput, or equivalence to Jev.
CPU and other backends remain outside this required acceptance.

### Optional native parity for other fixtures

The CPU and Metal full-decoder parity tests are ignored by default and never download a model.
Set `UZU_SCORE_MODEL` to an existing native model directory and select one explicitly:

```sh
UZU_SCORE_MODEL=/absolute/path/to/model cargo +nightly test -p uzu-engine native_metal_model_scores -- --ignored
# For the CPU reference, replace native_metal_model_scores with native_cpu_model_scores.
```
The SDK smoke script checks actual HTTP results. It does not prove model quality, calibration, or latency.

Earlier Metal parity passed on Qwen3.5-0.8B-M, with DeltaNet and attention layers; its CPU run was inconclusive.
Native parity uses prompt lengths 1, 1023, 1024, and 1025 against a separate 512-token prefill.
The Liquid acceptance above covers ShortConv only after a collected PASS on that exact model/backend.
Neither fixture exercises KV-shared trailing layers. That path needs its own native fixture validation.
