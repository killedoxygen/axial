# Telemetry
Axial has an anonymous, consent-gated telemetry path for broad product health signals. Local source builds and packaged builds without a valid PostHog key do not upload anything.

## What is collected
Telemetry events are defined only in `apps/api/src/observability/telemetry.rs`. The current event vocabulary is:

- `app_started`
  - `app_version`: the running app version
  - `os`: Rust's platform OS value
  - `arch`: Rust's platform architecture value
  - `active_flags`: registry flag keys whose local override differs from the default; dev-only keys are included only in debug builds
- `launch_started`
  - `loader_key`: the selected loader key, when present
- `launch_completed`
  - `outcome`: `success` or `failure`
- `instance_created`
  - `loader_key`: the selected loader key, when present
- `$exception`
  - `$exception_list`: exactly one object with `type` and `value`
  - `$exception_fingerprint`: the closed error kind, used for grouping
  - `$exception_level`: `error`, or `fatal` for panics
  - `area`: one of `launch`, `install`, `guardian`, `config`, `startup`, `panic`, or `frontend`

Every queued event also includes the anonymous install id as PostHog `distinct_id`, `$process_person_profile: false`, `environment`, and a UTC timestamp. `environment` is a deployment label, not a user property. The queue is local memory only, bounded, and flushed in batches when telemetry is configured and consent remains enabled.

`$exception` is used for privacy-safe backend error tracking in PostHog Error Tracking. Error kinds are a closed vocabulary in code, and summaries are short backend-authored labels or sanitized public copy. Axial deliberately never sends stack traces; Rust backtraces can include absolute user paths.

Frontend error reports use the same backend telemetry boundary at `/api/v1/telemetry/frontend-error`; the browser never talks to telemetry vendors directly. The frontend sends only the error kind (`error`, `unhandledrejection`, or `render`), the error constructor name, and a short truncated message. It does not send stacks, URLs, filenames, line numbers, or column numbers. The backend converts the report to the closed `frontend_error` `$exception` kind and sanitizes it through the same telemetry export redaction path as every other event.

## What is never collected
Telemetry must not include usernames, file paths, server addresses, instance names, tokens, hardware identifiers, command lines, account ids, or raw provider payloads.

Every property value is sanitized through the `TelemetryExport` redaction audience before it is queued. Values that look sensitive are dropped instead of uploaded. Events are sent to PostHog with `$process_person_profile: false`, so PostHog person profiles are not created for Axial telemetry events.

For `$exception`, the fingerprint and area are the durable signal. The summary is capped and sanitized; if it looks sensitive, the event is still sent with a redacted summary value.

## Identity
The telemetry identity is a random UUID install id stored in `config.json` as `telemetry_install_id`.

The id is committed with config only when telemetry consent is enabled and a valid export key is configured, before any event can be queued. It is not derived from hardware, usernames, paths, accounts, or any other local identity. Turning telemetry off clears the persisted install id and the in-memory queue. Turning telemetry on again commits a fresh id before event admission resumes.

## Consent
Telemetry is disabled by default in `AppConfig`. Onboarding includes a stats step with "Anonymous stats" and "Nothing sent" choices; that UI initializes to sharing unless the loaded config is explicitly false, but nothing is persisted until onboarding saves the config. Settings > Advanced has the same anonymous usage stats toggle.

When telemetry is off, events are not queued or newly admitted for export. Work admitted with the current identity before consent is revoked may finish, but revocation admits no new telemetry work.

Error tracking uses the same consent and key gates as every other telemetry event.

## Where it goes
Telemetry uses PostHog. The default host is the PostHog EU ingest endpoint, `https://eu.i.posthog.com`.

Uploads only happen when a valid `AXIAL_POSTHOG_API_KEY` is available at runtime or compiled into the build. The key must be a public PostHog project key with the `phc_` prefix. Keyless runs never upload.

`AXIAL_POSTHOG_HOST` can redirect the endpoint, including to a local or self-auditing PostHog-compatible endpoint. The host must be an `http` or `https` URL without credentials, query parameters, or fragments.

`AXIAL_POSTHOG_ENVIRONMENT` can override the deployment label attached to events. Values are lowercased and must contain only ASCII letters, numbers, hyphens, or underscores, up to 32 characters. Invalid values fall back to `dev` for debug builds and `production` otherwise.

## Error storm control
Backend error events are capped per process before they enter the telemetry queue. At most 30 `$exception` events are exported per process, and at most 5 events are exported for the same `$exception_fingerprint`. The counters reset only when the process restarts. Non-error telemetry events are unaffected.

## Panic capture
The backend installs a panic hook at startup. The hook records a single fatal `$exception` with kind `panic`, then chains the previous hook so normal stderr output remains.

Because the process may be exiting, panic capture does not rely on the async flush loop. It performs a best-effort single-event PostHog batch send on a fresh blocking thread with a short timeout. If telemetry consent or the PostHog key is absent, the hook is a no-op.
