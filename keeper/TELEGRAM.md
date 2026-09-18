# Optional Telegram notifications

Telegram is a best-effort operator convenience. It does not participate in proof construction, transaction submission, journal recovery or settlement. The keeper does not open a webhook or incoming port.

Set both `TELEGRAM_BOT_TOKEN` and numeric `TELEGRAM_CHAT_ID` privately to enable it. Leave both unset to disable it. A partial or malformed configuration returns a generic error without exposing either value; callers should disable the optional integration and continue randomness processing. Use a dedicated bot and a private operator chat. The module never prints tokens, authenticated URLs, response bodies or transport errors.

`TELEGRAM_LOW_BALANCE_WEI` optionally sets the low-funds threshold in native 18-decimal units. The default is `50000000000000000` (0.05 native USDC on Arc). This is a notification threshold, not a cost estimate or a transfer instruction. No funds are sent by this module.

## Integration API

Call `Settings::from_env(role)` and `TelegramNotifier::start(settings)` once inside the Tokio runtime. Retain the notifier for the process lifetime; cloning it is inexpensive. Dropping its final handle aborts both background tasks. `notify(Event)` uses a bounded 128-entry `try_send` channel and never awaits. Overflow is dropped and counted with a saturating counter. There is no durable notification queue or replay guarantee.

Emit `Event::Fulfilled { request_id, tx_hash }` only after successful receipt reconciliation confirms accepted randomness, not when a transaction is merely broadcast. Callback failure after valid proof acceptance is still a served result. Emit `OperationalError` with a fixed `ErrorClass`, never a raw RPC error, provider URL or exception string. Repeated errors and low-balance notices have five-minute in-memory cooldowns.

`ErrorClass::EpochPreparation` covers a deferred epoch fetch or publication and, since the same cooldown applies, live paid demand stuck on blocked or stale epoch work (the `epoch_stalled` health fault); inspect the selected source gateway and the keeper log for the epoch key and reason.

`Event::FeeBudget` reports a send deferred by a configured cap. It carries only the exceeded variable name (`MAX_GAS`, `MAX_FEE_PER_GAS_WEI`, `CANCEL_MAX_FEE_PER_GAS_WEI` or `MAX_TX_COST_WEI`), the required amount and the configured limit, and tells the operator to raise that cap and restart; the keeper never bypasses a cap to meet a deadline. The worker emits it from the fulfillment, epoch publication and replacement paths, and the status observer emits it whenever the observed base fee already prices sends (2 x base fee + 1 gwei) above `MAX_FEE_PER_GAS_WEI`. It shares the `ErrorClass::FeeBudget` five-minute cooldown, so a sustained fee spike produces one notice per cooldown, not one per tick.

Update `StatusSnapshot` from a separate, bounded maintenance task approximately every 60 seconds. It must use independently verified RPCs and journal/public state; it must not add an RPC await to request processing. The snapshot contains health, pending/served counts, last accepted serve time, current epoch ID and local/published state, public transaction wallet, chain ID, native balance and current epoch-committer authorization. `update_status` replaces the in-memory snapshot and enqueues a low-balance event when below threshold. If no observation is available, preserve explicit unknown values rather than guessing.

## Commands and delivery

`/status` returns the latest public operational snapshot and process uptime. `/keeper` returns the public transaction wallet on its own copyable line, chain, native balance, epoch-committer authorization and the wallet's explorer link when `EXPLORER_URL` is configured. Neither command returns any key, private RPC URL, database path or raw error. They are read-only and never trigger transactions, recovery, configuration changes or funding.

Commands are received by a separate outbound `getUpdates` long-poll task, started only when `TELEGRAM_COMMANDS` is true (the default for a primary keeper, false for a follower). Keepers that share a bot token compete for `getUpdates`, so exactly one of them may poll; a follower keeper can use the same token and chat in notification-only mode. Its messages start with `[follower]`, its takeover notices read `Follower served request N` and `Follower published epoch E`, and its `/status` text, if commands are enabled, names the role and whether it currently counts the primary as alive. It accepts only exact `/status`, `/keeper` or their suffix addressed to the bot's own username, from the configured numeric chat. Historical command dates before startup and unauthorized chats are ignored. The bootstrap offset begins at the latest update to avoid historical command storms. A dedicated bot should not have another poller or a pre-existing webhook; this module does not change webhook settings. For a group chat, any member of that configured chat can request these public summaries.

### Slash-command menu

Command handling and Telegram's visible slash menu are separate. Register the menu once when pairing a chat or changing the bot/chat configuration:

```sh
python3 scripts/telegram-commands.py --token-file /private/bot.token --config-file deploy/docker/keeper.env
python3 scripts/telegram-commands.py --token-file /private/bot.token --config-file deploy/docker/keeper.env --apply
```

The first call previews the current chat-scoped list. `--apply` registers only `/status` and `/keeper` for the configured chat and confirms Telegram's stored result. Private chats also receive a Commands menu button; groups use slash suggestions, including bot-addressed commands. Existing chat-specific English/Turkish overrides are kept consistent. Global command scopes, chat authorization, webhooks and the live poller are unchanged. Telegram retains this setting across keeper restarts; no daemon restart is needed. Credentials and chat identifiers are not printed.

The sender batches up to 16 queued events into messages below 3,900 bytes, spaces sends by at least one second, uses a three-second HTTP timeout and follows no redirects. HTTP 429 defers only that background sender for 30 seconds. Poll requests have a 10-second long-poll window, 15-second overall timeout and bounded 64 KiB response bodies; failures back off 30 seconds. Polling never blocks the independent sender. Delivery failure is best-effort loss, not a reason to retry or stall randomness work.

The balance and authorization are observations, not a funding or availability guarantee. Observe their freshness through the maintenance integration. Notifications can be dropped on overflow, shutdown, provider outage or rate limit. Receipt/journal and chain evidence remain authoritative.

References: [Telegram sendMessage](https://core.telegram.org/bots/api#sendmessage), [getUpdates](https://core.telegram.org/bots/api#getupdates), [rate limits](https://core.telegram.org/bots/faq#my-bot-is-hitting-limits-how-do-i-avoid-this).
