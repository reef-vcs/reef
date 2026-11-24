# Reef on your phone

A phone on your network can have the triage inbox ([inbox](inbox.md)), every proposal and its diff on its Home Screen. Your own daemon serves them; there is no app store, no account with Reef and no relay. The phone holds a token that only reads, and it ends when you say so. Its design is [ADR 0095](adr/0095-paired-devices.md).

For now the phone **reads only**. You accept, decline, answer and comment in the web UI on the computer Reef runs on. Deciding on the phone, with a passkey, and notifications arrive with the next two parts of R3.40.

## Set it up

You need a name for the daemon's machine that the phone reaches and a certificate that the phone trusts ([three ways](#names-and-certificates)). Then, on the daemon's machine:

1. Write the listener's settings in `config.toml` (`reef config show` prints where it is), and restart the daemon.

   ```toml
   [remote]
   enabled  = true
   listen   = "100.101.102.103:7470"                 # the machine's VPN address, or 0.0.0.0:7470
   names    = ["mac.example.ts.net"]
   tls_cert = "/Users/alice/.config/reef/mac.crt"
   tls_key  = "/Users/alice/.config/reef/mac.key"
   origin   = "https://mac.example.ts.net:7470"      # the one address the phone opens
   ```

2. Ask for a code.

   ```
   $ reef remote pair iphone
   Open   https://mac.example.ts.net:7470/
   Code   K7QX-9M2D          good for 10 minutes, once
   ```

   In a terminal it also prints the address as a QR code. The QR code never holds the code.

3. On the phone, open the address in Safari, tap **Share**, then **Add to Home Screen**. Open Reef from the Home Screen and type the code.

The app's storage is its own, so pair inside the app, not in Safari. **Pair this browser instead** is for a browser you want to read in as it is.

## What the phone can do

| | |
| --- | --- |
| Read | Every registered repository: the inbox, proposals with their evidence and diffs, the smartlog, workspaces, activity |
| Write | Your own "landed since you last looked" mark, and the snapshots a read may take: a read that names a workspace's change (`@`) first records what its agent has written, as any such read does, in an operation that names the phone |
| Where | Only at the `origin` it was paired at. When `origin` changes, pair again |
| Until | 90 days (`--expires`, 1 minute to 366 days), or until you end it |
| Who | The person it was paired for (`--identity`), your daemon's default person if you name none |

`/repos/<repo>/items/<key>` opens one inbox item wherever it is decided. For a help request that is its answer in the inbox, and an item that is gone says so.

## End it

```sh
reef remote token list               # paired devices have the scope `device`
reef remote token revoke iphone      # a lost phone: its open pages end within a second
```

**Unpair this phone**, in the app's settings, removes the token from the phone and ends its open pages. The daemon still lists the device until it expires; `revoke` ends it there, which you want when something else may hold the token. Pairing again under the same name ends the older device when the new one is paired, not before.

A backup holds the tokens. A backup older than a revocation brings the device back, so revoke it again after a restore.

## Names and certificates

The daemon's generated certificate does not work here: an exception that you make in Safari does not reach the Home Screen app. `reef remote pair` warns when the daemon uses it.

| Way | Trusted by the phone | Renewal | Costs |
| --- | --- | --- | --- |
| Tailscale | Yes, the certificate is Let's Encrypt's | 90 days, and renewing the files is yours | An account, the app on the phone, and the machine's name in public certificate logs |
| Your own domain, with any ACME client and DNS-01 | Yes | The client's timer | A domain, an API key for its DNS, and a route from the phone to the machine |
| A private CA, for example `mkcert` | Once you install its root on the phone and turn it on in Settings, General, About, Certificate Trust Settings | Yours | A CA key on your computer that can impersonate any site to that phone |

The daemon reads the two files again when either changes, at most once a minute, so a renewal needs no restart. `reef remote show` prints the days left. `reef daemon status` warns under 14.

**Tailscale.** Turn on MagicDNS and HTTPS certificates in the admin console, then write the files where `[remote]` names them, and run the same line weekly:

```sh
tailscale cert --cert-file ~/.config/reef/mac.crt --key-file ~/.config/reef/mac.key mac.example.ts.net
```

Set `listen` to the machine's tailnet IPv4 address (`tailscale ip -4`), so that the port is open on the tailnet alone and each device counts as its own address.

**Your own domain.** Point a name at an address the phone reaches, have the client write `fullchain.pem` and the key where `[remote]` names them, and put the name in `names` and `origin`.

**A private CA.** `mkcert -cert-file mac.crt -key-file mac.key mac.home.example`, with that name in the phone's DNS.

`tailscale serve` as a proxy in front of the listener is not supported: every client would share one address, and so one backoff after wrong tokens.

## Limits

- **Your computer asleep answers nothing.** A daemon on a machine that stays up is the answer.
- **The app notices a dead connection** and opens another: after a stream is silent for 40 s, or for 20 s when the app comes back to the screen or to the network. It then says **Reconnecting** until the inbox is current, and shows what it had. In Chromium and WebKit on a Mac the browser took 10 to 13 s more to give a dead connection up.
- **Until a new connection has said something, it is given longer**, since a slow link may need it: 40 s, and twice as long each time up to 320 s. After an outage of minutes with the app on screen all along, it can stay on **Reconnecting** for up to 320 s after the network is back, unless the phone tells the app that it is. Leave the app and come back: what waits then is given 40 s in all, and tried again.
- **A page reads again what it shows** when the app is live again, so **Live** can come a moment before the page's own read is back. A read that its connection failed is sent again by itself. A read in which no byte arrives for 40 s is cut and sent again, with the same longer limits, so a large diff loads over a slow link (tried at 20 kB/s).
- **An inbox that takes much over 40 s to arrive does not load.** 142 kB at 3.3 kB/s (43 s) did, and 283 kB at 3.3 kB/s and 1.4 MB at 20 kB/s (over 70 s) did not: it waits in one queue with what tells the page, the browser and the daemon that the connection lives, and one of them gives it up.
- **The app loads the code of its pages ahead** while it is live, so that a row opens though the connection died since. If the connection dies while code is on its way, as on the app's first start, the page says that loading it takes long after 10 s, or that it could not be loaded. Press **Reload** there.
- **Five tabs at once**, if you read in a browser: each has three live streams, and a token may hold 16.
- **Nothing is readable offline**, on purpose: the phone keeps no copy of your code. It keeps the token, its name and its end.
- **A program that runs as you** can read the TLS key, stop the daemon and serve its own page at the phone's address, which the app then opens with its token ([threat model](threat-model.md), S-281). A daemon on another machine, or under another user, closes that.
- **Wrong codes slow their address down.** After 10 in a minute the daemon closes new connections from that address for a minute, and longer each time. A paired phone there keeps its pairing and reconnects when that ends. Over IPv6 every peer in one /64 counts as one address, so let `listen` name the IPv4 address.
- **A link does not change a paired phone's session.** A page opened with a token in its link keeps the phone's own. Unpair first to use the link.
- **What the app was tested on.** The browser tests run the pages at 375 by 812 in Chromium, Firefox and WebKit. The Home Screen app was tried as a prototype in the iOS 26.1 Simulator. On an iPhone (iOS 27.2.1, over Tailscale) a throwaway server of the same shape loaded trusted, its storage survived a restart of the phone, and links opened the exact page. No real phone has run this code yet.
