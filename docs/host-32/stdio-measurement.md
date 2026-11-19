# H1 standard ACP stdio measurement

The Darwin arm64 Preview runtime pinned by Studio `844e6b30:scripts/studio-preview/baseline.json` ran the installed adapter `3bdb9451` and native wheels built from `e19ab55e`. The model endpoint was a synthetic loopback fixture. The probe counted UTF-8 bytes on the actual adapter pipes and excluded line separators.

| Standard exchange | Host → adapter bytes | Largest adapter → host line in bytes |
| --- | ---: | ---: |
| Text with Unicode, a quote and backslash | 193 | 217 |
| Prompt image at the adapter's 8 MiB decoded limit | 11,184,985 | 217 |
| Large successful text resource | 16,773,119 | 217 |
| Native `read` image tool result | 193 | 4,719,325 |
| Resource at the adapter transport input boundary | 16,777,215 | 217 |
| Same session loaded with history replay | 216 | **16,777,274** |

The input boundary resource returned `-33002` after dispatch and appeared in load replay. The tool result contained 3,538,941 decoded image bytes, the largest base64 input accepted by the default native image resize fast path. The adapter's stdin frame limit is 16 MiB including the separator. Verified in `aelix-acp@3bdb9451:src/aelix_acp/wire/transport.py`, `src/aelix_acp/session.py` and `src/aelix_acp/server.py`, and `aelix-ai@e19ab55e:packages/aelix-coding-agent/src/aelix_coding_agent/util/image_resize.py`.

H1 sets the common Aelix stdio line limit to **32 MiB** in both directions. This gives nearly 16 MiB of headroom over the largest observed line. A limit of 16 MiB would cut off the recorded standard load replay. Other ACP backends retain their existing framing. The relay's 256 KiB limit is applied separately to the full correlated extension response. Host tests cover the observed replay size and the common boundary, including CRLF. The scripted ACP peer separately tests the 256 KiB boundary with Unicode, a quote and a backslash.

Run [the probe](../../scripts/verification/measure_aelix_acp_frames.py) using Python 3.11 or newer. Pass `--bundle` the prepared directory with the receipt and installed binaries, and `--fixture` the pinned Studio `scripts/studio-preview/fixtures/loopback.py`. The [measurement receipt](stdio-measurement.json) includes the wheel digests. The measurements cover the pinned standard surfaces above and a loopback model. Custom tools and arbitrarily sized future adapter outputs are constrained by the Host line limit.
