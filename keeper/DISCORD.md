# Public Discord proof feed

The optional Discord integration posts accepted proofs to one explicitly configured protocol channel. It sends no errors, balance/funding requests, health reports, local epoch preparation messages or administrative commands. Telegram remains the operator channel.

Set `DISCORD_BOT_TOKEN`, `DISCORD_PROTOCOL_CHANNEL_ID` and `EXPLORER_URL` privately. The last value is the chain explorer's public HTTPS base URL. Optionally set `DISCORD_PUBLIC_EXPLORER_URL` to the deployed D20DAO website base URL to add a direct request replay link. Do not use a localhost preview or a credential-bearing URL. Application ID and public key are unnecessary for this outbound REST transport.

Messages contain the request ID, chain, epoch ID, accepted randomness, proof hash and transaction link. They are emitted only after finalized canonical receipt reconciliation and durable nonce/job resolution. The heading says “Proof verified”: an accepted proof is not a claim that the consumer callback succeeded. Callback errors are not posted. There is no separate epoch broadcast or historical replay on startup.

The sender has a bounded 128-event memory queue and no network await in the keeper tick. Delivery is best-effort: overflow, crashes and ambiguous HTTP failures can lose a notification. HTTP 429 waits for the server's delay. Transient transport/5xx failures and rate limits allow at most two retries within a 60-second attempt window, with the identical payload and a deterministic nonce using Discord's recent-message deduplication. Other HTTP errors are not retried. Transport status is recorded only in private process logs, never posted to Discord. Rate limiting and connection failures cannot hold the nonce lane. Messages disable all mentions and link previews, so `Embed Links` and `Mention Everyone` are unnecessary.

Install the bot through Guild Install with the `bot` scope. Keep Public Bot disabled if only the app owner should install it. The protocol channel requires `View Channel` and `Send Messages` (permission integer 3072). Explicit channel/category restrictions can deny these even when requested by the installation link. No administrator permission, webhook, Gateway session, message-content intent or incoming port is needed for the proof feed.

Copying existing community messages is a separate operator task, not a daemon feature. It additionally requires `Read Message History` (combined integer 68608) and temporarily enabling Message Content Intent to read other authors' text. Keep the original text and do not delete the author's messages without authorization. Those read capabilities can be removed after the copy is complete.

Configure the optional variables in the existing Docker `keeper.env`, preserving all keys, journals and implementation pins. Restart using the existing service wrapper after deploying the reviewed binary. Never run a second keeper instance with the same wallet.
