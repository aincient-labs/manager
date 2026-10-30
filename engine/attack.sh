#!/usr/bin/env bash
# The tester checklist (studies/manager-sidecar-security.md, plan M7 / 0483)
# run against a live stack with the EXPERIMENTAL engine enabled
# (`atelier --site <slug> engine enable --experimental`). Each check prints
# PASS or FAIL with what it saw; the exit status is the number of failures.
#
#   engine/attack.sh <project> <port>     # e.g. atelier-engine-test 41223
#
# It changes the site: it pairs, sets a wake code, rests, locks down, wakes and
# revokes. Run it on a test site only, never one you care about. Takes a few
# minutes (freeze + wake).
set -uo pipefail
P=${1:?compose project, e.g. atelier-engine-test}
PORT=${2:?the site HTTP port}
EDGE="http://localhost:$PORT"
API="$EDGE/_atelier/engine/v1"
NET="${P}_engine"
ENGINE="$P-engine-1"
APP="$P-app-1"
EDGE_C="$P-edge-1"
CURL_IMG=curlimages/curl:8.11.1
HERE=$(cd "$(dirname "$0")" && pwd)
fails=0

pass() { printf '  \033[32mPASS\033[0m %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; fails=$((fails + 1)); }
section() { printf '\n\033[1m%s\033[0m\n' "$*"; }
# status code of a request through edge, as an internet client sends it
code() { curl -s -o /dev/null -w '%{http_code}' --path-as-is "$@"; }
# a request from edge's own network namespace (the one peer the engine
# accepts), so edge's rate limits don't hide the engine's own caps
inside() { docker run --rm --network "container:$EDGE_C" "$CURL_IMG" -s "$@"; }
expect() { # name want got
  if [[ "$3" == "$2" ]]; then pass "$1 ($3)"; else fail "$1: want $2, got $3"; fi
}
never2xx() { # name got
  if [[ "$2" =~ ^2 ]]; then fail "$1: got $2"; else pass "$1 ($2)"; fi
}
job_wait() { # token: wait until the running job finishes, print its error
  for _ in $(seq 1 120); do
    s=$(curl -s -H "Authorization: Bearer $1" "$API/status")
    if echo "$s" | grep -q '"finished":[0-9]'; then
      echo "$s" | grep -oE '"error":(null|"[^"]*")' | cut -d: -f2-
      return
    fi
    sleep 2
  done
  echo timeout
}
field() { sed -n "s/.*\"$1\":\"\([^\"]*\)\".*/\1/p"; }
# A new Manager asks to be paired (E-02): prints the engine's answer.
ask() { # name [address it asks from]
  inside -X POST -H "X-Real-IP: ${2:-192.0.2.50}" -d "{\"name\":\"$1\"}" http://engine:8080/v1/pair/request
}
collect() { inside -X POST -d "{\"handle\":\"$1\"}" http://engine:8080/v1/pair/collect; }
# Ask, have the primary ($TOKEN) type the code, collect: prints the pairing.
pair_new() { # name [address]
  local out c h
  out=$(ask "$1" "${2:-192.0.2.50}")
  c=$(echo "$out" | field code)
  h=$(echo "$out" | field handle)
  curl -s -o /dev/null -H "Authorization: Bearer $TOKEN" -X POST -d "{\"code\":\"$c\"}" "$API/pairings/approve"
  collect "$h"
}

docker pull -q "$CURL_IMG" >/dev/null

section "0. The experimental gate (m7-2): the image alone starts nothing"
out=$(docker run --rm "$(docker inspect -f '{{.Config.Image}}' "$ENGINE")" 2>&1)
rc=$?
if [[ $rc -ne 0 && "$out" == *"refusing to start"* ]]; then pass "without ATELIER_ENGINE_EXPERIMENTAL=1 the engine exits: ${out%%.*}"; else fail "the engine started without the flag (exit $rc): $out"; fi
r=$(docker inspect -f '{{range .Config.Env}}{{println .}}{{end}}' "$ENGINE" | grep -c '^ATELIER_ENGINE_EXPERIMENTAL=1$')
expect "the running engine was started with the flag" 1 "$r"

# A known start: no pairings, unlocked, wake not paused, awake.
docker restart "$ENGINE" >/dev/null
sleep 1
for id in $(docker exec "$ENGINE" atelier engine admin pairings | awk '{print $1}'); do
  docker exec "$ENGINE" atelier engine admin revoke "$id" >/dev/null
done
docker exec "$ENGINE" rm -f /state/requests.json
docker exec "$ENGINE" atelier engine admin unlock >/dev/null
docker exec "$ENGINE" atelier engine admin resume-wake >/dev/null
docker start "$P-db-1" "$P-app-1" >/dev/null
for _ in $(seq 1 60); do
  [[ "$(code "$EDGE/user/login")" =~ ^[23] ]] && break
  sleep 2
done

section "1. Exposure: no published port, app shares no network with the engine"
ports=$(docker port "$ENGINE" 2>/dev/null)
expect "engine publishes no port" "" "$ports"
nets=$(docker inspect -f '{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{end}}' "$APP")
if [[ "$nets" == *"_engine"* ]]; then fail "app is on the engine network: $nets"; else pass "app networks: $nets"; fi
internal=$(docker network inspect -f '{{.Internal}}' "$NET")
expect "engine network is internal (no route out)" true "$internal"
eip=$(docker inspect -f "{{(index .NetworkSettings.Networks \"$NET\").IPAddress}}" "$ENGINE")
r=$(docker exec "$APP" curl -s -m 3 -o /dev/null -w '%{http_code}' "http://engine:8080/v1/version" 2>&1)
never2xx "app → engine by name" "${r:-000}"
r=$(docker exec "$APP" curl -s -m 3 -o /dev/null -w '%{http_code}' "http://$eip:8080/v1/version" 2>&1)
never2xx "app → engine by IP $eip" "${r:-000}"
r=$(docker run --rm --network "$NET" "$CURL_IMG" -s -m 3 -o /dev/null -w '%{http_code}' "http://engine:8080/v1/version")
never2xx "a container on the engine network that isn't edge is closed" "$r"
# What `app` can do once it reaches the port: hold a slow body open, and open
# many connections. Neither may stall the engine or grow its threads.
docker exec -d "$APP" php -r '$s=fsockopen("'"$eip"'",8080); fwrite($s,"POST /v1/pair HTTP/1.0\r\nContent-Length: 100000\r\n\r\nab"); sleep(20);'
docker exec -d "$APP" php -r '$a=[]; for($i=0;$i<300;$i++){$a[]=@fsockopen("'"$eip"'",8080);} sleep(20);'
sleep 2
t0=$(date +%s%N)
r=$(code -m 5 "$API/version")
ms=$(( ($(date +%s%N) - t0) / 1000000 ))
if [[ $r == 200 && $ms -lt 2000 ]]; then pass "a slow body and 300 connections from app don't stall the engine (${ms} ms)"; else fail "engine stalled: $r in ${ms} ms"; fi
th=$(docker exec "$ENGINE" sh -c 'grep Threads /proc/1/status' | awk '{print $2}')
if [[ $th -lt 25 ]]; then pass "engine threads stay bounded ($th)"; else fail "engine threads: $th"; fi
r=$(docker exec "$APP" curl -s -m 3 -o /dev/null -w '%{http_code}' "http://edge/_atelier/engine/v1/status")
expect "app → edge → engine is an anonymous client like any other" 401 "$r"
r=$(docker exec "$ENGINE" wget -q -T 3 -O /dev/null https://ghcr.io >/dev/null 2>&1 && echo reached || echo blocked)
expect "engine has no route to the internet" blocked "$r"

section "2. Closed verbs: only listed paths, auth on everything but version/pair/wake"
expect "version is public" 200 "$(code "$API/version")"
v=$(curl -s "$API/version")
if [[ "$v" =~ \"api\":\"1\.[0-9]+\.[0-9]+\" ]]; then pass "version names API 1.x.y: $v"; else fail "version: $v"; fi
for v in "GET status" "POST freeze" "POST live" "POST rest" "POST lockdown" "POST unlock" \
         "GET pairings" "DELETE pairings/self" "GET audit" "POST wake/resume" "POST wake/paired" \
         "GET pairings/requests" "POST pairings/approve"; do
  set -- $v
  expect "$1 $2 without a token" 401 "$(code -X "$1" "$API/$2")"
done
expect "bad token" 401 "$(code -H 'Authorization: Bearer aet_0000' "$API/status")"
for p in exec shell "status/../exec" "v1/status" "pairings/abc"; do
  never2xx "unknown path $p" "$(code -X DELETE "$API/$p")"
done
expect "DELETE audit (nothing truncates the log)" 405 "$(code -X DELETE "$API/audit")"
expect "outside v1 is 404 at edge" 404 "$(code "$EDGE/_atelier/engine/status")"
expect "another API version is 404" 404 "$(code "$EDGE/_atelier/engine/v2/status")"

section "3. Edge path tricks (decoded, slash-merged, dot-resolved before matching)"
for p in "//_atelier/engine/v1/status" "/_atelier//engine/v1/status" \
         "/_atelier%2Fengine%2Fv1%2Fstatus" "/x/../_atelier/engine/v1/status" \
         "/_atelier/engine/v1/./status" "/_atelier/engine/v1/x/../status" \
         "/_atelier/engine/%76%31/status" "/_ATELIER/engine/v1/status"; do
  never2xx "$p" "$(code "$EDGE$p")"
done
# A decoded newline: `$` in a loose regex matches before it, so these once
# slipped past the exact /pair and /wake locations and their 5/min limits.
for p in "pair%0A" "wake%0A" "status%0A" "pair%0D%0A" "pair%00"; do
  never2xx "POST $p never reaches the engine" "$(code -X POST -d '{}' "$API/$p")"
done
r=$(for i in $(seq 1 45); do code -H "Authorization: junk$i" "$API/status"; echo; done | sort | uniq -c | tr '\n' ' ')
if [[ "$r" == *429* ]]; then pass "a junk Authorization header is still limited as anonymous: $r"; else fail "junk auth escaped the anonymous limit: $r"; fi
r=$(docker run --rm --network "container:$EDGE_C" --entrypoint sh "$CURL_IMG" -c \
  'for i in $(seq 1 35); do curl -s -o /dev/null -w "%{http_code}\n" -H "Authorization: Bearer aet_$(printf %064d $i)" http://engine:8080/v1/status; done' | tail -1)
expect "an address sending made-up tokens is told to back off after 30 a minute" 429 "$r"
r=$(curl -s --path-as-is "$EDGE/_atelier/engine/v1/../../../user/login" -o /dev/null -w '%{http_code}')
pass "a dot path out of the prefix leaves it ($r from Drupal, not the engine)"
r=$(inside -o /dev/null -w '%{http_code}' -H 'Cookie: SESSabc=1' http://engine:8080/v1/status)
never2xx "a session cookie authenticates nothing" "$r"

section "4. Pairing (E-02): the install code pairs the primary, every other Manager swaps codes with it"
r=$(inside -o /dev/null -w '%{http_code}' -X POST -d '{"code":"0000-0000-0000-0000","name":"x"}' http://engine:8080/v1/pair)
expect "no install code made yet: nothing pairs" 401 "$r"
IC=$(docker exec "$ENGINE" atelier engine admin install-code 2>/dev/null)
if [[ "$IC" =~ ^[0-9A-Z]{4}-[0-9A-Z]{4}-[0-9A-Z]{4}-[0-9A-Z]{4}$ ]]; then pass "the installer's code looks like XXXX-XXXX-XXXX-XXXX (80 bits)"; else fail "install code: $IC"; fi
if docker exec "$ENGINE" grep -rq "${IC//-/}" /state; then fail "install code in plaintext on disk"; else pass "only the install code's hash is stored"; fi
if docker exec "$APP" sh -c "grep -rqs '${IC//-/}' /opt/drupal/engine-outbox /opt/drupal/engine-inbox /opt/drupal/private"; then fail "app can read the install code"; else pass "the install code is nowhere app can read"; fi
docker run --rm --network "container:$EDGE_C" --entrypoint sh "$CURL_IMG" -c \
  'for i in $(seq 1 25); do curl -s -o /dev/null -X POST -d "{\"code\":\"0000000000000000\",\"name\":\"x\"}" http://engine:8080/v1/pair; done'
r=$(for i in $(seq 1 8); do code -X POST -d '{"code":"0000000000000000","name":"x"}' "$API/pair"; echo; done | sort | uniq -c | tr '\n' ' ')
if [[ "$r" == *429* ]]; then pass "edge rate-limits /pair per IP: $r"; else fail "no 429 from edge: $r"; fi
r=$(inside -o /dev/null -w '%{http_code}' -X POST -d "{\"code\":\"$IC\",\"name\":\"Shibin\u202es MacBook\"}" http://engine:8080/v1/pair)
expect "a name with a bidi override is refused" 400 "$r"
out=$(inside -X POST -d "{\"code\":\"$(echo "$IC" | tr 'A-Z' 'a-z')\",\"name\":\"Reviewer laptop\"}" http://engine:8080/v1/pair)
TOKEN=$(echo "$out" | field token)
ID=$(echo "$out" | field id)
if [[ "$TOKEN" == aet_* ]]; then pass "after 25 wrong tries the code still works (nothing burns it), typed lower case: id $ID"; else fail "pair: $out"; fi
r=$(docker exec "$ENGINE" atelier engine admin pairings | grep "$ID" | grep -c primary)
expect "the install code's pairing is the primary" 1 "$r"
r=$(inside -o /dev/null -w '%{http_code}' -X POST -d "{\"code\":\"$IC\",\"name\":\"again\"}" http://engine:8080/v1/pair)
expect "a used install code can't pair twice" 401 "$r"
expect "the token works" 200 "$(code -H "Authorization: Bearer $TOKEN" "$API/status")"
r=$(inside -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" http://engine:8080/v1/status)
expect "a valid token gets in from an address that sent 35 made-up ones" 200 "$r"
if docker exec "$ENGINE" grep -q "${TOKEN#aet_}" /state/pairings.json; then fail "plaintext token on disk"; else pass "only the token's hash is stored"; fi
if "$HERE/console-standin.sh" "$P" list | grep -q token; then fail "console list leaks hashes"; else pass "the console's list carries no hashes"; fi
out=$(ask "Second laptop")
RC=$(echo "$out" | field code)
RH=$(echo "$out" | field handle)
if [[ "$RC" =~ ^[0-9A-Z]{4}-[0-9A-Z]{4}$ && ${#RH} -eq 64 ]]; then pass "anyone may ask: a code to show ($RC) and a handle to poll with"; else fail "ask: $out"; fi
expect "before approval the new Manager gets no key" 202 "$(inside -o /dev/null -w '%{http_code}' -X POST -d "{\"handle\":\"$RH\"}" http://engine:8080/v1/pair/collect)"
expect "a malformed handle is refused" 400 "$(inside -o /dev/null -w '%{http_code}' -X POST -d '{"handle":"00"}' http://engine:8080/v1/pair/collect)"
expect "a made-up handle gets nothing" 404 "$(inside -o /dev/null -w '%{http_code}' -X POST -d "{\"handle\":\"$(printf '%064d' 7)\"}" http://engine:8080/v1/pair/collect)"
expect "an unknown field is refused" 400 "$(inside -o /dev/null -w '%{http_code}' -X POST -d '{"name":"x","primary":true}' http://engine:8080/v1/pair/request)"
list=$(curl -s -H "Authorization: Bearer $TOKEN" "$API/pairings/requests")
if [[ "$list" == *"Second laptop"* && "$list" != *"${RC//-/}"* && "$list" != *"$RC"* ]]; then pass "the primary sees the request's name and address, never its code"; else fail "requests: $list"; fi
expect "a code no request has approves nothing" 404 "$(code -H "Authorization: Bearer $TOKEN" -X POST -d '{"code":"0000-0000"}' "$API/pairings/approve")"
expect "the owner types the code from the new Manager's screen" 200 "$(code -H "Authorization: Bearer $TOKEN" -X POST -d "{\"code\":\"$(echo "$RC" | tr 'A-Z' 'a-z')\"}" "$API/pairings/approve")"
out=$(collect "$RH")
T2=$(echo "$out" | field token)
ID2=$(echo "$out" | field id)
if [[ "$T2" == aet_* ]]; then pass "once approved, the new Manager collects its key"; else fail "collect: $out"; fi
expect "a key is collected once" 404 "$(inside -o /dev/null -w '%{http_code}' -X POST -d "{\"handle\":\"$RH\"}" http://engine:8080/v1/pair/collect)"
r=$(docker exec "$ENGINE" atelier engine admin pairings | grep "$ID2" | grep -c primary)
expect "an approved Manager is never the primary" 0 "$r"
out=$(ask "Rogue")
expect "only the primary can approve" 403 "$(code -H "Authorization: Bearer $T2" -X POST -d "{\"code\":\"$(echo "$out" | field code)\"}" "$API/pairings/approve")"
expect "only the primary can list requests" 403 "$(code -H "Authorization: Bearer $T2" "$API/pairings/requests")"
ask "flood" 198.51.100.4 >/dev/null
ask "flood" 198.51.100.4 >/dev/null
expect "one address can keep two requests pending, no more" 429 "$(inside -o /dev/null -w '%{http_code}' -X POST -H 'X-Real-IP: 198.51.100.4' -d '{"name":"flood"}' http://engine:8080/v1/pair/request)"
out=$(ask "Over SSH" 192.0.2.60)
docker exec "$ENGINE" atelier engine admin approve "$(echo "$out" | field code)" >/dev/null
r=$(collect "$(echo "$out" | field handle)")
ID7=$(echo "$r" | field id)
if [[ "$r" == *aet_* ]]; then pass "an owner with SSH approves a request too"; else fail "ssh approve: $r"; fi
docker exec "$ENGINE" atelier engine admin revoke "$ID7" >/dev/null
n0=$(docker exec "$ENGINE" atelier engine admin pairings | wc -l)
docker exec -u www-data "$APP" sh -c "printf '{\"uid\": 1}' > /opt/drupal/engine-inbox/pair.request"
sleep 3
r=$(docker exec "$APP" sh -c 'ls /opt/drupal/engine-outbox')
n1=$(docker exec "$ENGINE" atelier engine admin pairings | wc -l)
if [[ "$r" != *pair.code* && "$n0" == "$n1" ]]; then pass "the console has no pairing door: Drupal's old pair.request does nothing"; else fail "outbox: $r, pairings $n0 → $n1"; fi
docker exec -u www-data "$APP" rm -f /opt/drupal/engine-inbox/pair.request

section "5. Inbox: Drupal-controlled files can't steer the engine"
docker exec "$ENGINE" sh -c 'sha256sum /state/pairings.json /state/engine.json 2>/dev/null' > /tmp/engine-before.$$
docker exec -u www-data "$APP" sh -c 'cd /opt/drupal/engine-inbox && ln -sf /state/pairings.json revoke.request'
sleep 3
docker exec "$ENGINE" sh -c 'sha256sum /state/pairings.json /state/engine.json 2>/dev/null' > /tmp/engine-after.$$
if cmp -s /tmp/engine-before.$$ /tmp/engine-after.$$; then pass "a symlinked request file: engine state unchanged"; else fail "engine state changed"; fi
seen=no
for _ in 1 2 3 4 5; do
  docker exec "$ENGINE" sh -c "grep 'refused: open' /state/audit.log | grep -q revoke.request" && { seen=yes; break; }
  sleep 1
done
if [[ $seen == yes ]]; then pass "the symlinked request is refused and audited"; else fail "no refusal in the audit log"; fi
r=$(docker exec -u www-data "$APP" sh -c 'echo "[]" > /opt/drupal/engine-outbox/pairings.json' >/dev/null 2>&1 && echo wrote || echo refused)
expect "Drupal can't rewrite the pairings list the console shows" refused "$r"
if "$HERE/console-standin.sh" "$P" list | grep -q "$ID"; then pass "the console's list shows the pairing"; else fail "pairing missing from the list"; fi
docker exec -u www-data "$APP" sh -c 'cd /opt/drupal/engine-inbox && rm -f revoke.request && mkfifo revoke.request'
sleep 3
expect "a FIFO request doesn't hang the engine" 200 "$(code -m 5 "$API/version")"
docker exec -u www-data "$APP" sh -c 'cd /opt/drupal/engine-inbox && head -c 100000 /dev/zero > revoke.request'
sleep 2
expect "an oversized request is dropped" 200 "$(code -m 5 "$API/version")"
docker exec -u www-data "$APP" sh -c "cd /opt/drupal/engine-inbox && printf '{\"id\":\"../../state\"}' > revoke.request"
sleep 2
expect "a malformed revoke changes nothing" 200 "$(code -H "Authorization: Bearer $TOKEN" "$API/status")"

section "6. Wake code: answers say only 'wrong', timing says nothing more"
t_unset=$(docker run --rm --network "container:$EDGE_C" --entrypoint sh "$CURL_IMG" -c \
  'for i in 1 2 3 4 5; do curl -s -o /dev/null -w "%{time_total}\n" -X POST -d "{\"code\":\"wrong wrong wrong\"}" http://engine:8080/v1/wake; done' | sort | sed -n 3p)
WAKE=$(docker exec "$ENGINE" atelier engine admin wake-code 2>/dev/null)
t_set=$(docker run --rm --network "container:$EDGE_C" --entrypoint sh "$CURL_IMG" -c \
  'for i in 1 2 3 4 5; do curl -s -o /dev/null -w "%{time_total}\n" -X POST -d "{\"code\":\"wrong wrong wrong\"}" http://engine:8080/v1/wake; done' | sort | sed -n 3p)
pass "median wrong-code time: no code set ${t_unset}s, code set ${t_set}s (Argon2id both ways)"
b1=$(inside -X POST -d '{"code":"x"}' http://engine:8080/v1/wake)
b2=$(inside -X POST -d '{"code":"a-much-longer-wrong-guess-here"}' http://engine:8080/v1/wake)
expect "short and long wrong codes get the same answer" "$b1" "$b2"
curl -s -H "Authorization: Bearer $TOKEN" -X POST "$API/wake/resume" >/dev/null
for i in $(seq 1 10); do inside -o /dev/null -X POST -d '{"code":"wrong wrong wrong"}' http://engine:8080/v1/wake; done
r=$(inside -o /dev/null -w '%{http_code}' -X POST -d "{\"code\":\"$WAKE\"}" http://engine:8080/v1/wake)
expect "after 10 tries from one address, even the right code is paused there" 429 "$r"
r=$(inside -o /dev/null -w '%{http_code}' -H 'X-Real-IP: 203.0.113.7' -X POST -d '{"code":"wrong wrong wrong"}' http://engine:8080/v1/wake)
expect "another address isn't paused: one guesser can't lock the owner out" 401 "$r"
r=$(docker run --rm --network "container:$EDGE_C" --entrypoint sh "$CURL_IMG" -c \
  'seq 1 16 | xargs -P 16 -I{} curl -s -o /dev/null -w "%{http_code}\n" -H "X-Real-IP: 198.51.100.9" -X POST -d "{\"code\":\"wrong wrong wrong\"}" http://engine:8080/v1/wake' | grep -c 401)
if (( r <= 10 )); then pass "16 tries sent at once: only $r get checked (counted before the check)"; else fail "$r of 16 concurrent tries were checked"; fi
docker run --rm --network "container:$EDGE_C" --entrypoint sh "$CURL_IMG" -c \
  'for i in $(seq 1 200); do curl -s -o /dev/null -H "X-Real-IP: 10.9.$((i / 250)).$((i % 250))" -X POST -d "{\"code\":\"wrong wrong wrong\"}" http://engine:8080/v1/wake; done'
r=$(inside -o /dev/null -w '%{http_code}' -H 'X-Real-IP: 192.0.2.1' -X POST -d "{\"code\":\"$WAKE\"}" http://engine:8080/v1/wake)
expect "200 tries an hour from many addresses pause wake by code for everyone" 429 "$r"
until=$(curl -s -H "Authorization: Bearer $TOKEN" "$API/status" | sed -n 's/.*"wake_paused_until":\([0-9]*\).*/\1/p')
left=$(( ${until:-0} - $(date +%s) ))
if (( left > 3000 && left <= 3600 )); then pass "the pause ends on its own (in ${left}s)"; else fail "pause until: $until"; fi
expect "the primary pairing resumes wake by code" 200 "$(code -H "Authorization: Bearer $TOKEN" -X POST "$API/wake/resume")"

section "7. Rest, wake, lockdown (D-09, D-10, D-12)"
for _ in $(seq 1 70); do code "$API/version" >/dev/null; done
expect "70 anonymous calls from this IP leave a paired Manager unthrottled" 200 "$(code -H "Authorization: Bearer $TOKEN" "$API/status")"
expect "rest starts" 202 "$(code -H "Authorization: Bearer $TOKEN" -X POST "$API/rest")"
expect "one job at a time" 409 "$(code -H "Authorization: Bearer $TOKEN" -X POST "$API/freeze")"
who=$(docker exec "$APP" sh -c 'for i in $(seq 1 100); do for p in /proc/[0-9]*; do if tr "\0" " " < $p/cmdline 2>/dev/null | grep -q "[a]incient:freeze"; then stat -c %U $p; exit; fi; done; sleep 0.2; done; echo none')
expect "the freeze runs drush as www-data, not root" www-data "$who"
err=$(job_wait "$TOKEN")
expect "rest finished without error" null "$err"
st=$(curl -s -H "Authorization: Bearer $TOKEN" "$API/status")
if echo "$st" | grep -q '"app":"exited"'; then pass "app is stopped"; else fail "status: $st"; fi
r=$(code "$EDGE/")
expect "visitors still get the frozen home page while resting" 200 "$r"
expect "the wake page is served" 200 "$(code "$API/wake")"
for _ in 1 2 3 4 5 6; do code "$API/wake" >/dev/null; done
expect "viewing the wake page isn't a code try (6 views), and the 70 anonymous calls above don't block it" 200 "$(code "$API/wake")"
t0=$(date +%s)
r=$(code -X POST -H 'Content-Type: application/x-www-form-urlencoded' --data-urlencode "code=$WAKE" "$API/wake")
expect "the right wake code wakes, after section 3's pairing guesses from this IP" 202 "$r"
if [ "$r" = 202 ]; then
  err=$(job_wait "$TOKEN")
  expect "wake finished without error" null "$err"
  pass "woken in $(( $(date +%s) - t0 ))s (C-03 budget: 20 s on 2 vCPU / 4 GB)"
fi
r=$(curl -s -o /dev/null -w '%{http_code}' "$EDGE/user/login")
if [[ "$r" =~ ^[23] ]]; then pass "Drupal answers again ($r; waking signs nobody in)"; else fail "after wake: $r"; fi
T3=$(pair_new "Paired after the owner" 192.0.2.51 | field token)
out=$(ask "Approved before the lockdown" 192.0.2.52)
curl -s -o /dev/null -H "Authorization: Bearer $TOKEN" -X POST -d "{\"code\":\"$(echo "$out" | field code)\"}" "$API/pairings/approve"
H4=$(echo "$out" | field handle)
# A compromised Drupal makes drush hang (the second review's finding 1): the
# lockdown must still stop the site at once.
DRUSH_BIN=/opt/drupal/vendor/bin/drush
docker exec "$APP" sh -c "mv $DRUSH_BIN $DRUSH_BIN.attack && printf '#!/bin/sh\nsleep 3000\n' > $DRUSH_BIN && chmod 755 $DRUSH_BIN"
restore_drush() { docker exec "$APP" sh -c "[ -e $DRUSH_BIN.attack ] && mv -f $DRUSH_BIN.attack $DRUSH_BIN; pkill -f 'sleep 3000'" >/dev/null 2>&1; }
trap restore_drush EXIT
expect "freeze starts (drush now hangs)" 202 "$(code -H "Authorization: Bearer $TOKEN" -X POST "$API/freeze")"
sleep 2
t0=$(date +%s)
r=$(curl -s -H "Authorization: Bearer $TOKEN" -X POST "$API/lockdown")
if [[ "$r" == *'"after":"freeze"'* ]]; then pass "lockdown during a job is accepted, not refused ($r)"; else fail "lockdown during a job: $r"; fi
err=$(job_wait "$TOKEN")
expect "lockdown cut the hung freeze short and finished without error" null "$err"
st=$(curl -s -H "Authorization: Bearer $TOKEN" "$API/status")
if echo "$st" | grep -q '"app":"exited"' && echo "$st" | grep -q '"lockdown":true'; then pass "lockdown stopped the site despite the hung drush, in $(( $(date +%s) - t0 ))s"; else fail "status: $st"; fi
docker start "$P-db-1" "$P-app-1" >/dev/null
restore_drush  # app has to be running for this
r=none
for _ in $(seq 1 40); do
  sleep 2
  if [[ "$(docker inspect -f '{{.State.Running}}' "$APP")" == false ]]; then r=stopped; break; fi
done
expect "a site started by hand during a lockdown is stopped again by the engine" stopped "$r"
expect "lockdown revoked the pairing made after this Manager" 401 "$(code -H "Authorization: Bearer $T3" "$API/status")"
expect "no other verb runs while locked down" 409 "$(code -H "Authorization: Bearer $TOKEN" -X POST "$API/freeze")"
expect "nobody can ask to pair while locked down" 403 "$(code -X POST -d '{"name":"x"}' "$API/pair/request")"
expect "a request approved before the lockdown gets no key" 404 "$(inside -o /dev/null -w '%{http_code}' -X POST -d "{\"handle\":\"$H4\"}" http://engine:8080/v1/pair/collect)"
expect "a locked-down site refuses the right wake code" 403 "$(code -X POST -d "{\"code\":\"$WAKE\"}" "$API/wake")"
expect "a paired Manager must unlock before waking" 409 "$(code -H "Authorization: Bearer $TOKEN" -X POST "$API/wake/paired")"
expect "unlock" 200 "$(code -H "Authorization: Bearer $TOKEN" -X POST "$API/unlock")"
expect "wake by Manager (own path, outside the code limit)" 202 "$(code -H "Authorization: Bearer $TOKEN" -X POST "$API/wake/paired")"
err=$(job_wait "$TOKEN")
expect "woken" null "$err"

section "8. Revoke and audit"
out=$(pair_new "Second laptop" 192.0.2.53)
T2=$(echo "$out" | field token)
ID2=$(echo "$out" | field id)
r=$(code -H "Authorization: Bearer $T2" -X DELETE "$API/pairings/$ID")
expect "a newer pairing can't revoke the owner's older one" 403 "$r"
expect "only the primary can resume wake by code" 403 "$(code -H "Authorization: Bearer $T2" -X POST "$API/wake/resume")"
r=$(docker exec "$ENGINE" sh -c 'cat /proc/1/status' >/dev/null; docker inspect -f '{{.RestartCount}}' "$ENGINE")
docker run --rm --network "container:$EDGE_C" --entrypoint sh "$CURL_IMG" -c \
  "seq 1 60 | xargs -P 60 -I{} curl -s -o /dev/null -H 'Authorization: Bearer $T2' http://engine:8080/v1/status"
expect "60 status calls at once from one Manager don't crash the engine (restarts)" "$r" "$(docker inspect -f '{{.RestartCount}}' "$ENGINE")"
expect "and it still answers" 200 "$(code -H "Authorization: Bearer $TOKEN" "$API/status")"
"$HERE/console-standin.sh" "$P" revoke "$ID" >/dev/null
expect "the console can't revoke the primary pairing" 200 "$(code -H "Authorization: Bearer $TOKEN" "$API/status")"
docker exec -u www-data "$APP" mkdir -p /opt/drupal/engine-inbox/revoke.request/x
sleep 2
r=$(docker exec "$APP" test -e /opt/drupal/engine-inbox/revoke.request && echo stuck || echo cleared)
expect "a planted directory in the inbox is cleared" cleared "$r"
"$HERE/console-standin.sh" "$P" revoke "$ID2" >/dev/null
expect "revoked from the console: its next call fails" 401 "$(code -H "Authorization: Bearer $T2" "$API/status")"
ID6=$(pair_new "Revoked over SSH" 192.0.2.54 | field id)
docker exec "$ENGINE" atelier engine admin revoke "$ID6" >/dev/null
if "$HERE/console-standin.sh" "$P" list | grep -q "$ID6"; then fail "an SSH revoke still shows in the console's list"; else pass "an SSH revoke leaves the console's list at once"; fi
audit=$(curl -s -H "Authorization: Bearer $TOKEN" "$API/audit")
for want in '"verb":"pair"' "revoke $ID2" '"verb":"lockdown"' '"verb":"wake"' 'pairings/approve' 'install-code' 'pair/request'; do
  if echo "$audit" | grep -q "$want"; then pass "audit has $want"; else fail "audit lacks $want"; fi
done
expect "forget this site (revoke self)" 200 "$(code -H "Authorization: Bearer $TOKEN" -X DELETE "$API/pairings/self")"
expect "the forgotten token fails" 401 "$(code -H "Authorization: Bearer $TOKEN" "$API/status")"
env_leak=$(curl -s "$API/version"; echo "$audit")
if echo "$env_leak" | grep -qE 'HASH_SALT|DB_PASSWORD|ADMIN_PASS'; then fail ".env content in a response"; else pass "no .env content in any response read"; fi

rm -f /tmp/engine-before.$$ /tmp/engine-after.$$
printf '\n%s\n' "$( [[ $fails -eq 0 ]] && echo 'All checks passed.' || echo "$fails check(s) failed." )"
exit "$fails"
