# Using agent-chat from an AI agent

`agent-chat` is an XMPP client with a CLI built for agents. A background daemon
holds the connection; every command talks to it and prints **one line of JSON**.
The first command you run starts the daemon automatically.

## Get an account

An agent can create its own account on servers that support proof-of-work
registration (our Prosody module `mod_register_pow`; for a hub on your own
machine, see [Run your own hub](#run-your-own-hub)):

```sh
agent-chat account register --server example.org          # generated username + password
agent-chat account register --server example.org --username my-agent
```

It solves the server's proof-of-work challenge (about 10 s on a laptop),
prints progress as JSON lines on stderr and one result object on stdout, stores
the password in the keychain (`--store config` on headless machines), and logs
in. Exit codes: 0 registered, 2 timeout, 5 refused by the server, 6 username
taken. With an existing account, use `agent-chat account add JID` instead.

## Run your own hub

`agent-chat` includes a small XMPP server, the **hub**, so people and agents on
one machine can chat without any outside server. One hub serves every profile
on the machine. Its files live in `$AGENT_CHAT_HOME/hub/`.

**1. Start the hub and get your own account.**

```sh
agent-chat hub start                      # the first run creates a hub for the domain `localhost`
agent-chat account add --hub my-agent --store config   # creates my-agent@localhost and logs in
```

`account add --hub NAME` starts the hub if needed, generates a password and
stores it. Exit code 6 means the name is taken.

**2. Create an account for the person you work with.** Pick a name, create the
account, and give them the output:

```sh
agent-chat hub user add alice --format text
# account:  alice@localhost
# password: 7Kq…   (shown once)
# server:   127.0.0.1, port 5222 (STARTTLS)
```

The person signs in with their own profile, for example
`agent-chat --profile alice account add alice@localhost --host 127.0.0.1 --insecure-plaintext --password-stdin`
(plaintext is only allowed on this machine), then uses the CLI or
`agent-chat-gui` with that profile. Or let them create the account themselves
in one step: `agent-chat --profile alice account add --hub alice`.

**3. Start the conversation.** Once their client is connected, say hello so
they see you, then wait for the reply:

```sh
agent-chat send alice@localhost "Hi! I'm your agent on this machine. Message me here whenever you need me."
agent-chat wait --from alice@localhost --timeout 600
```

The hub doesn't store messages for offline accounts yet: if the person isn't
connected, it can't deliver the message (it answers with an error instead), so
send the greeting after they have signed in.

Other hub commands: `hub status` (sessions, accounts, listeners), `hub user
list|passwd NAME|del NAME`, `hub stop`. A hub for the local network:
`agent-chat hub init --lan` (domain `<this machine's name>.local`).

## Receive messages

```sh
agent-chat wait --timeout 300        # block until the next message, print it, exit 0
                                     # exit 2 = timed out, nothing printed
agent-chat inbox                     # all unread messages as a JSON array (marks them read)
agent-chat inbox --peek              # look without marking read
agent-chat listen                    # stream messages forever, one JSON object per line
```

Each message is printed exactly once per **consumer**. If several agents share an
account, give each one its own name: `--consumer planner` (or set
`AGENT_CHAT_CONSUMER`). Your own outgoing messages are never returned, so
replying can't loop back to you. Filters: `--from bob@example.org`,
`--room team@conference.example.org`.

```json
{"id":42,"conversation":"bob@example.org","kind":"chat","from":"bob@example.org",
 "body":"can you summarise the PR?","timestamp":"2026-11-10T16:15:38Z","delayed":false,
 "source":"live","own":false,
 "encryption":{"protocol":"omemo2","sender_device":1479488527,"trust":"trusted"}}
```

- `conversation` is where to reply.
- `kind` is `chat` (1:1), `groupchat` (room) or `muc_private`.
- `delayed: true` means the message arrived late: offline delivery or the server archive.
- `encryption` is `null` for plaintext. When `encryption.error` is set, `body` is only a placeholder.

## Reply

```sh
agent-chat send bob@example.org "Here's the summary: ..."
echo "long text" | agent-chat send bob@example.org        # body from stdin
agent-chat send team@conference.example.org "hi all"     # rooms are detected automatically
```

Messages are end-to-end encrypted (OMEMO) whenever the recipient supports it.
In private rooms every message is encrypted for all members' devices; public
rooms are plaintext.
`--require-encryption` fails (exit 4) instead of sending plaintext.
`--plaintext` disables encryption.

## A complete agent loop

```sh
while true; do
  msg=$(agent-chat wait --timeout 600) || continue    # exit 2 on timeout: wait again
  to=$(jq -r .conversation <<<"$msg")
  body=$(jq -r .body <<<"$msg")
  reply=$(your-agent "$body")
  agent-chat send "$to" "$reply"
done
```

## Other commands

| Command | Purpose |
|---|---|
| `agent-chat status` | Connection state |
| `agent-chat history JID --limit 20` | Recent conversation history |
| `agent-chat conversations` | Conversations with their last message |
| `agent-chat contacts` / `contacts add JID` / `contacts approve JID` | Roster |
| `agent-chat rooms join ROOM@SERVICE --nick NAME` / `rooms` | Group chats |
| `agent-chat rooms join ROOM@SERVICE --private` | Create a private room (members-only); its messages are end-to-end encrypted |
| `agent-chat rooms invite ROOM JID` / `rooms invites` / `rooms decline ROOM` | Invite someone; list or ignore invitations you received |
| `agent-chat presence away "busy"` | Presence |
| `agent-chat omemo devices [JID]` / `omemo trust JID FINGERPRINT` | Encryption devices and trust |

## Exit codes

| Code | Meaning |
|---|---|
| 0 | OK |
| 1 | Error |
| 2 | `wait` timed out |
| 3 | Not connected, or no account configured |
| 4 | Encryption required but unavailable |

## Trust

New devices are trusted until you verify one (Blind Trust Before Verification).
Once any device of a contact is verified with `omemo trust`, that contact's new
devices are untrusted until verified. Use `wait --require-trusted` to only act
on messages from trusted devices.
