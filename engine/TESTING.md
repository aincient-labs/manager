# Testing the Atelier engine: a kit for outside security testers

Thank you for testing this. This kit says what the engine is, what we would like you to attack,
the rules, how to set up a server for it, the checklist, what we already know, and how to tell us
what you find.

Version under test: **manager commit `<COMMIT>`** <!-- owner: fill in when the channel is chosen (plan m7-6) -->.
How you get it: **`<CHANNEL>`** <!-- owner: a build shared with testers only, or a public experimental tag that is never `latest` or `edge` (m7-6) -->.
You also receive the design, `manager-sidecar-security.md`. Its threats table, floors and tester
checklist are the reference for everything below.

## 1. What this is, and what it isn't

Atelier runs as a Docker stack on the owner's server: `edge` (nginx, the only published port),
`app` (Drupal) and `db`. The **engine** is a fourth container that lets an owner's Manager (the
`atelier` CLI or the desktop app) look after a site on a server without SSH: pair, rest, wake,
lock down, revoke. To do that it mounts `/var/run/docker.sock`. **That is root on the server.**

- **Experimental.** Nothing turns it on by default. It lives in its own compose file
  (`compose.engine.yaml`), only `atelier engine enable --experimental` writes that file, and the
  engine refuses to start unless `ATELIER_ENGINE_EXPERIMENTAL=1` is set, which only that file does.
  The Manager hides every engine command behind `--experimental` (DECISIONS 0483).
- **Never on a production box.** Use a throwaway server you own and can wipe. Assume anything on
  it can be lost.
- **Your findings decide.** The engine becomes the default only after every testing party's
  findings have an answer. Until then it ships to testers and nobody else.
- **What the engine is not yet.** At `<COMMIT>` its API has no `backup`, `restore`, `update`,
  `export` or `doctor` verbs (the design lists them; they come later). The console's Managers
  screen is not in a released Atelier yet; `engine/console-standin.sh` plays its part (it reads
  and writes the engine's inbox and outbox as Drupal would).

## 2. Scope

**In scope**

- The engine API through edge, at `https://<your-site>/_atelier/engine/v1/*`: auth, the closed
  list of verbs, argument handling, our own HTTP layer (`crates/aincient-engine/src/http.rs`),
  connection and process caps.
- Pairing: the install code, the code swap for later Managers, collect-once, seniority among
  pairings (who can revoke whom), the primary.
- Wake: the wake code, its rate limits and pauses, timing and response differences, `/wake/paired`.
- Lockdown: what it revokes and stops, what still works during it, how it ends.
- The inbox/outbox file protocol **from a compromised Drupal**: assume you are `www-data` (or root)
  in the `app` container and try to steer, crash, mislead or reach the engine.
- The Manager client: HTTPS-only, redirect handling, the API version check, what it sends where.
- Edge routing: path tricks against the `/_atelier/engine/` prefix, and whether its rules hold
  awake, frozen, resting and locked down.
- The trusted-proxy path (`ATELIER_TRUSTED_PROXIES`, `X-Forwarded-For`, the loopback publish).
- Resting-mode routing: what edge serves while `app` and `db` are stopped.
- The typed-code pairing flow as a **design question**: could a compromised console, an email or a
  fake page talk an owner into approving an attacker's code? Is the residual acceptable?

**Out of scope**

- Drupal core or contrib vulnerabilities that don't involve the engine (report those through the
  [atelier-cms security policy](https://github.com/aincient-labs/atelier-cms/security/policy)).
- Denial of service by volume (floods that need bandwidth, not a flaw). Cheap single-client
  exhaustion of the engine *is* in scope.
- Your own infrastructure: the VPS provider, your proxy, your OS.
- Social engineering of us or of any real owner. The typed-code flow above is in scope as design.

## 3. Rules of engagement

- Test only on servers you own and installed for this. Never another person's Atelier install, and
  never a server we run (`aincient-labs.com`, `try.aincient-labs.com` or any test host of ours).
- No automated scanning of `aincient-labs.com` or anything under it. The installer download is fine.
- Keep findings private until they are fixed and a disclosure date is agreed. Don't share them with
  other testing parties; we fix in one channel for everyone (section 7).
- Disclosure: **90 days after your report, or when the fix ships, whichever is first**
  <!-- owner: confirm the window -->. If we need longer, we ask you, with a reason.
- Credit: you are named in the advisory and the changelog when the fix ships, the way you want to
  be named, or not at all if you prefer.

## 4. Setting up a test server

What we used: a 2 vCPU / 4 GB VPS, Ubuntu, Docker Engine with the Compose plugin, and a domain
pointing at it (an `sslip.io` name works). Commands below run as a normal user `atelier` in the
`docker` group, except where `root` is shown. Replace `test.example.org` with your domain.

**1. Server and Docker.** Install Docker Engine and the Compose plugin
([docs.docker.com/engine/install](https://docs.docker.com/engine/install/)), then:

```sh
# root
adduser --disabled-password atelier && usermod -aG docker atelier
```

**2. Close the site port first.** Atelier publishes edge on port 41221. Ports Docker publishes
**bypass host firewalls** such as `ufw`, and on our VPS the port was also open over **IPv6**
through `docker-proxy`. Until step 6 moves it to loopback, drop it on both families:

```sh
# root (not persisted across reboots unless you save them)
iptables  -I DOCKER-USER -i eth0 -p tcp -m conntrack --ctorigdstport 41221:41299 -j DROP
ip6tables -I INPUT       -i eth0 -p tcp --dport 41221:41299 -j DROP
```

**3. Install Atelier** with the released installer (it writes `~/.atelier` and starts edge, app, db):

```sh
# atelier
curl -fsSL https://aincient-labs.com/atelier/install.sh | bash
```

**4. Build the engine image and the Linux CLI from `<COMMIT>`.** The engine image is built on the
server and never pulled; the CLI binary inside it is the same build, so you can copy it out.

<!-- owner: replace <SOURCE> with how testers get the source (m7-6) -->

```sh
# atelier
git clone <SOURCE> manager && cd manager && git checkout <COMMIT>
engine/build.sh                       # docker build → atelier-engine:dev (local only)
mkdir -p ~/.local/bin
docker create --name atelier-cli atelier-engine:dev
docker cp atelier-cli:/usr/local/bin/atelier ~/.local/bin/atelier && docker rm atelier-cli
atelier --version
```
<!-- verify: copying the CLI out of the engine image (static musl build) was not what our VPS run did;
we built it with `docker run --rm -v "$PWD":/src -w /src rust:1-bookworm cargo build --release --locked -p atelier`.
Either should work; the second is the one we ran. -->

**5. A TLS proxy.** The Manager talks to the engine over HTTPS only, so put a proxy in front. Ours
was Caddy (official image, host network), with this `Caddyfile`:

```
test.example.org {
	reverse_proxy 127.0.0.1:41221
}
```

```sh
# atelier, in the directory holding the Caddyfile
docker run -d --name caddy --network host --restart unless-stopped \
  -v "$PWD/Caddyfile:/etc/caddy/Caddyfile:ro" -v caddy_data:/data caddy:2.10
```
<!-- verify: exact `docker run` for Caddy; the run used Caddy 2.10.2's official image on the host network. -->

Caddy reaches edge through `docker-proxy`, so edge sees every proxied request coming from the stack
network's gateway. Tell the engine to believe `X-Forwarded-For` from that address and nobody else:

```sh
# atelier
docker network inspect -f '{{range .IPAM.Config}}{{.Gateway}}{{end}}' atelier_default   # e.g. 172.18.0.1
echo 'ATELIER_TRUSTED_PROXIES=172.18.0.1' >> ~/.atelier/.env
```

IPs or CIDRs, comma or space separated; `0.0.0.0/0` and the like are refused. Never list an address
an untrusted client can arrive as.

**6. Enable the engine.** It pins the image you built by its ID, writes `compose.engine.yaml` and
`edge.engine.conf`, restarts the stack and prints the **install code** (`XXXX-XXXX-XXXX-XXXX`,
shown once) that pairs your first Manager. Keep it private. The released installer wrote the
stack files from the released templates, and `enable` refuses while they differ from this build's;
`doctor --fix` rewrites them (your `.env` values, such as `ATELIER_TRUSTED_PROXIES`, are kept).

```sh
# atelier
atelier doctor --fix
atelier engine enable --experimental
ss -ltn | grep 41221            # expect 127.0.0.1:41221 only, no 0.0.0.0 or [::]
atelier doctor                  # the "(EXPERIMENTAL)" lines should all be [x]
curl -s https://test.example.org/_atelier/engine/v1/version
```

With `ATELIER_TRUSTED_PROXIES` set, `enable` publishes edge on `127.0.0.1` only and refuses if
Compose would publish it anywhere else; the proxy is then the only way in. Without it, edge stays
on all addresses and every client is its own TCP peer. A new install code, any time:
`docker exec atelier-engine-1 atelier engine admin install-code`.

**7. Pair a Manager.** On your own computer, build the CLI from `<COMMIT>` (`cargo build --release
--locked -p atelier`; the binary is `target/release/atelier`) and pair it as the primary. It asks
for the install code; the token goes to the OS keychain.

```sh
atelier sites add t1 --url https://test.example.org --name "Tester laptop" --experimental
atelier --site t1 app status --experimental
atelier --site t1 engine managers --experimental
```
<!-- verify: the keychain on a Linux desktop (Secret Service) was not exercised; our runs used macOS. -->

**8. Optional: a second Manager.** On another computer or OS user (they would share a keychain
otherwise), ask to join; it shows an 8-symbol code and waits up to 10 minutes:

```sh
atelier sites join t1b --url https://test.example.org --name "Second laptop" --experimental
```

On the primary, see the request (name, address, age; never its code) and type the code the new
Manager shows:

```sh
atelier --site t1 engine requests --experimental
atelier --site t1 engine approve --experimental
```

Over SSH the same is `docker exec atelier-engine-1 atelier engine admin requests|approve <CODE>`.

**9. Resting and the wake code.**

```sh
atelier --site t1 engine wake-code --rotate --experimental   # shown once
atelier --site t1 site rest --experimental                   # freeze, then stop app and db
# visitors get the snapshot; https://test.example.org/atelier shows the wake form
atelier --site t1 site wake --experimental
atelier --site t1 site lockdown --experimental               # hidden command; unlock: site unlock, or SSH
```

SSH-side owner commands, without a token: `docker exec atelier-engine-1 atelier engine admin --help`
(`pairings`, `revoke`, `make-primary`, `lockdown`, `unlock`, `rest`, `wake`, `resume-wake`, `audit`,
`status`). The console stand-in: `engine/console-standin.sh atelier list|revoke <id>`.

**Teardown.** `atelier engine remove --experimental` on the server takes the engine off and puts
today's stack back (its volumes stay). Then wipe the server.

## 5. The checklist

Answer each line in writing: held, broke (with a report), or not tried. The pointers say where to
start; go further where it looks soft. The audit log is `docker exec atelier-engine-1 atelier engine
admin audit`.

**Exposure and peers**
- [ ] `app` reaches the engine by any route: network, IP across bridges, shared volume, edge
      loopback, `X-Accel-Redirect`. Try from `docker exec atelier-app-1 …` (as `www-data` and root).
- [ ] The engine accepts a TCP peer other than edge. Connect from another container or the host.
- [ ] The engine publishes a port, or edge publishes beyond loopback behind a trusted proxy (`ss -ltn`, IPv4 and IPv6).

**Closed API**
- [ ] A request field reaches a `docker` or `docker compose` argument. Read `server.rs` `route` and
      every subprocess call (`proc.rs`, `site.rs`); try odd ids, names, JSON types, unknown fields.
- [ ] A verb other than `version`, `pair`, `pair/request`, `pair/collect`, `wake` answers without a token.
- [ ] A response carries `.env` content, a token hash or a code.
- [ ] `http.rs` misreads what edge or a hostile peer sends: slow bodies, huge headers, pipelining,
      chunked bodies, 300 connections, many parallel `status` calls from one token.

**Edge**
- [ ] Path tricks reach an unnamed route or skip a rate-limit bucket: `//_atelier`, `%2F`, `..`,
      `%0A`, case, trailing slashes, `;`, double encoding. Awake, frozen, resting and locked down.
- [ ] A made-up `Authorization` header escapes the anonymous limit; anonymous traffic starves a
      paired Manager or the wake page.
- [ ] Edge follows something `app` controls into the engine's prefix or out of the docroot
      (response headers, the `frozen/current` symlink; see section 6).

**Pairing (E-02)**
- [ ] Anything Drupal can write, show or read leads to a pairing. The inbox carries only `revoke.request`.
- [ ] The install code: entropy, storage (hash only, in `engine-state`), who can read it, whether
      guesses burn it, edge's 5/min per address on `/pair`.
- [ ] Code swap: two pending per address and 32 in all, 10 minutes each, collect once, the handle,
      an approved request collected by someone else, a request created during lockdown.
- [ ] Names: control, bidi and zero-width characters, look-alikes that pose as another pairing.
- [ ] Seniority: a junior pairing revokes, outranks or survives the primary or a senior one; the
      primary chosen implicitly (idle expiry, re-pairing); a race between lockdown and a new pairing.
- [ ] The typed-code flow as design: could an owner be talked into approving an attacker's code? Is
      the residual acceptable? (Design, Pairing item 7.)

**Wake and lockdown**
- [ ] A wake response or its timing says more than "wrong" (no code set vs set, short vs long).
- [ ] The pauses: 10 tries an hour per address, 200 an hour from everywhere, 1 h doubling to 24 h,
      kept across engine restarts. Can concurrent tries slip past? Can one address keep the owner out?
- [ ] The wake code unlocks anything but wake.
- [ ] Lockdown is refused, runs anything of Drupal's, leaves `app` or `db` up (hand-start them, restart
      the engine mid-lockdown, lock down during a running job), or lets a pairing in.
- [ ] Subprocesses (drush, compose) hang a job, exhaust memory or the process cap, or outlive their deadline.

**Inbox, outbox and audit**
- [ ] As Drupal, plant symlinks, FIFOs, huge files, directories or races in the inbox; write the outbox.
- [ ] The console's list (outbox `pairings.json`) can be made to hide or invent a pairing.
- [ ] The audit log can be edited, flushed or hidden from the API side; a revoke is missing from it.

**Manager client and trusted proxy**
- [ ] The Manager sends a token or a code over `http://` (other than loopback), follows a redirect,
      or talks to an engine with a newer API major.
- [ ] A client picks the address it is audited and rate-limited by (`X-Forwarded-For`, `X-Real-IP`,
      IPv6, a path around the proxy).

**Two questions for you as reviewers**
- [ ] Is "no way back from lockdown without a Manager or SSH" acceptable?
- [ ] Would an appliance mode of an existing tool (for example Scotty) fit as the engine better than our own?

### Running `engine/attack.sh`

`attack.sh` is our own checklist as a script, about 150 checks. Run it on the server, as the user
that owns `~/.atelier`, from the checkout of `<COMMIT>`. It needs `docker` and `curl` and pulls
`curlimages/curl:8.11.1`.

```sh
# atelier, in ~/manager
EXPECT_IP=<this server's public IP> engine/attack.sh atelier 41221 https://test.example.org
```

- `atelier` is the compose project (`~/.atelier`), `41221` the site port, the third argument the
  public HTTPS base. Without it, it uses `http://localhost:41221`.
- `EXPECT_IP` is the address edge should see for the machine running the script. Behind a proxy on
  the same server, the script's requests leave for the public address and come back through the
  proxy, so that is the server's own public IP. Section 2b then checks the audit log names exactly it.
- **It changes the site:** it revokes every pairing, clears pending requests, mints a new install code,
  sets a new wake code, rests, locks down, wakes and revokes. Pair your Managers again afterwards.
- A pass is every line `PASS` and the exit status `0` (the number of failures). Our last run on a
  VPS through Caddy passed every check.

**It is a floor, not the test.** It shows the build in front of you does what we already thought
of. The point of this round is what we didn't.

## 6. Known limits

Already known, so not new findings. Deepening one (a worse impact, a cheaper path, a way to chain it)
is a new finding.

1. **Host processes can claim an address.** Behind the proxy, every connection to edge's loopback
   port arrives as the Docker gateway, so a process on the server can set `X-Forwarded-For` to any
   address. We treat the host as the owner's.
2. **Lockdown serves the last snapshot**, which a compromised Drupal may have made. An engine-owned
   static page would not be.
3. **Edge follows `app`'s `frozen/current` symlink** to serve the snapshot; `app` writes it.
   `disable_symlinks` can't be on, because the snapshot *is* a symlink.
4. **Audit entries name every actor's IP, and every pairing can read the audit**, so one Manager sees
   the others' addresses.
5. **A killed `compose exec` may leave drush running** inside `app` until `app` stops.
6. **The console (Drupal) can revoke every non-primary Manager** through the inbox. It can't revoke
   the primary or add one.
7. **The engine image is pinned by its local image ID**, not a registry digest, and is not signed.
   It is built on your server from source; publishing and signing come later.
8. **Wake is slower than the 20 s budget on the released Atelier** (about 27 s on 2 vCPU / 4 GB).
   A faster restart exists in an unreleased cms build. Not a security issue.
9. **Many addresses can keep an owner without a Manager from waking by code** until the pause ends
   (24 h at most). Accepted: an outage, not a breach.
10. **No way back from a lockdown without a Manager or SSH.** That is its purpose.
11. **The wake code is 16 symbols**, not the four diceware words the design names (no word list yet).
12. **Bearer tokens**, not signed requests; HTTPS is required. Signing is later hardening.
13. **Docker Desktop and OrbStack route between bridge networks**, so network separation alone
    doesn't keep `app` off the engine; the engine's edge-only peer check does.
14. **`attack.sh`'s wake timing check is coarse** (five tries each way). Measure properly if you care.
15. **The site port is open until the engine is enabled with a trusted proxy** (section 4, step 2),
    and the firewall rules there aren't persisted.

## 7. How to report

Report privately through **GitHub private vulnerability reporting** on the manager repository, as
`SECURITY.md` says: the [Security tab](https://github.com/aincient-labs/manager/security) →
**Report a vulnerability**. Not in issues, pull requests or discussions. <!-- owner: if testers
without GitHub need another path, add the contact here -->

Include:

- the commit (`<COMMIT>`) and `atelier --version`, on the server and the Manager;
- how your setup differs from section 4 (OS, Docker and Compose versions, proxy, firewall);
- the steps, as commands, from a fresh setup;
- what an attacker gets, and from where they start (internet, `app`, a paired Manager, the host);
- the evidence: responses, `docker exec atelier-engine-1 atelier engine admin audit` lines, edge or
  engine logs (`docker logs atelier-edge-1`, `atelier-engine-1`). Mask tokens and codes.

What happens next:

- We acknowledge within 3 business days and give a first assessment within 7, as `SECURITY.md` says.
- Every finding gets a written answer under our decision for the engine's design (DECISIONS 0482):
  fixed, changed in the design, or accepted with the reason.
- Fixes ship to every testing party in the same channel as this build, as a new commit, with what
  changed. Please re-test what you reported.
- When it is fixed and the disclosure date comes, we publish an advisory (ID format
  `ATELIER-SA-YYYY-NNN`) with your credit.

### Report template

```
Title:
Commit / versions:       <COMMIT>, atelier --version (server, Manager)
Setup deltas:            OS, Docker/Compose, proxy, ATELIER_TRUSTED_PROXIES, firewall
Starting position:       internet | app container (www-data/root) | paired Manager (primary/junior) | host
Checklist line(s):
Steps:
  1.
  2.
Expected:
Observed:
Impact:                  what the attacker gets; which floor or threat row it breaks
Evidence:                responses, audit lines, logs (tokens and codes masked)
Severity (your view):
Credit:                  name / handle / anonymous
```
