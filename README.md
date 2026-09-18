# fake-sms-server

Twilio-compatible fake SMS gateway for 1KM development. Receives what
the 1KM server's Twilio variant sends, returns realistic SIDs, and
shows everything in a web inbox — no provider account needed.

```text
POST /2010-04-01/Accounts/:sid/Messages.json  Twilio shape (form To/From/Body, Basic auth tolerated)
POST /send                                     custom bridge shape (JSON to/body/sender?)
GET  /api/messages                            inbox, newest first (JSON)
GET  /healthz                                 liveness
GET  /                                        web inbox UI (auto-refresh)
```

Ephemeral by design: bounded in-memory inbox (1000), restarts wipe it.
Never expose publicly — auth is accepted, never verified.

## Run

```bash
cargo run --release            # :8080 (HOST/PORT env)
docker build -t fake-sms . && docker run -p 8080:8080 fake-sms
```

## Pointing the 1KM server at it

```bash
SMS_PROVIDER=twilio
TWILIO_ACCOUNT_SID=test-sid
TWILIO_AUTH_TOKEN=test-token
TWILIO_FROM=+15550000000
TWILIO_BASE_URL=http://localhost:8080   # or http://fake-sms:8080 in compose
```

Then OTP + lifecycle SMS land in the inbox instead of the server log.
