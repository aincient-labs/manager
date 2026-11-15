#!/usr/bin/env bash
# state-zoo.sh — real Atelier sites in the states real sites end up in (plan
# manager-scenario-tests § M3, DECISIONS 0528). Local only: it runs the real
# atelier-cms image on this computer's Docker (OrbStack / Docker Desktop), the
# runtime CI can't cover. README § State zoo says what each state imitates.
#
#   scripts/state-zoo.sh [--pull] up [state…]   create zoo-<state> sites (all by default)
#   scripts/state-zoo.sh down [state…]          remove every zoo-* site (or these) completely
#   scripts/state-zoo.sh status                 list the zoo sites and their containers
#   scripts/state-zoo.sh [--pull] check <atelier args…>
#                                               run `atelier --site zoo-<state> <args…>` on
#                                               each, print state / exit / outcome / time
#                                               (ZOO_ONLY="state …" limits it to those)
#
# It drives the branch's own CLI (target/debug/atelier, built first), never the
# one on PATH. Hard rails: only sites zoo-*, compose projects atelier-zoo-*,
# containers atelier-zoo-*, image tags atelier-cms:zoo-*; never prunes; never
# restarts Docker; never pulls a new atelier-cms image without --pull; the active
# site is put back if anything changed it.
#
# Written for the macOS system bash (3.2): no associative arrays, no mapfile.

set -u -o pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
BIN="$REPO/target/debug/atelier"
LOGS="$REPO/zoo-logs"
SITES_DIR="$HOME/.atelier/sites"
REGISTRY="$HOME/.atelier/sites.toml"
IMAGE="ghcr.io/aincient-labs/atelier-cms"
LATEST="$IMAGE:latest"
SHARED_TAG="atelier-cms:zoo-shared"
ENGINE_DEV_IMAGE="atelier-engine:dev"
OLD_COMPOSE="$REPO/crates/aincient-core/templates/compose-0.14.1.yaml"
OLD_EDGE="$REPO/crates/aincient-core/templates/edge-0.14.0.conf"

# Every state, in the order `up` builds them (restored-elsewhere last: it
# borrows a backup from a site made before it).
STATES="old-template shared-tag-a shared-tag-b engine-resting stopped detached-db ro-backups digest-pinned restored-elsewhere"

# The owner's sites. Never addressed, whatever a pattern says.
PROTECTED="default kelp-kiln kelp-kiln-rec foo-bar-baz engine-poc review-tmp hardening-test"

# Per running site, measured on OrbStack (2026-11-15): idle ≈ 110 MiB, first
# boot higher; disk ≈ 180 MiB of volumes + one backup. Rounded up.
RAM_PER_SITE_MIB=400
DISK_PER_SITE_MIB=300
DISK_FLOOR_GIB=5

PULL=0

# --- output ---------------------------------------------------------------------

say() { printf '%s\n' "$*"; }
warn() { printf 'zoo: %s\n' "$*" >&2; }
die() {
	warn "$*"
	exit 1
}

# --- the rails ------------------------------------------------------------------

is_protected() {
	local s
	for s in $PROTECTED; do [ "$1" = "$s" ] && return 0; done
	return 1
}

# A site slug the zoo may touch: zoo-<something>, never an owner's site.
zoo_slug() {
	case "$1" in
	zoo-?*) ;;
	*) die "refused: '$1' is not a zoo-* site" ;;
	esac
	is_protected "$1" && die "refused: '$1' is one of the owner's sites"
	return 0
}

zoo_project() {
	case "$1" in
	atelier-zoo-?*) ;;
	*) die "refused: compose project '$1' is not atelier-zoo-*" ;;
	esac
}

zoo_container() {
	case "$1" in
	atelier-zoo-?*) ;;
	*) die "refused: container/network '$1' is not atelier-zoo-*" ;;
	esac
}

zoo_tag() {
	case "$1" in
	atelier-cms:zoo-?*) ;;
	*) die "refused: image tag '$1' is not atelier-cms:zoo-*" ;;
	esac
}

zoo_home() {
	zoo_slug "$1"
	printf '%s/%s' "$SITES_DIR" "$1"
}

# The branch's CLI against one zoo site. Always --site: the active site is never
# what a zoo command acts on.
atelier_on() {
	local slug=$1
	shift
	zoo_slug "$slug"
	NO_COLOR=1 "$BIN" --site "$slug" "$@" </dev/null
}

# `docker compose` for a zoo site's own project, in its own folder.
compose_on() {
	local slug=$1 home
	shift
	home=$(zoo_home "$slug") || exit 1
	zoo_project "atelier-$slug"
	(cd "$home" && docker compose -p "atelier-$slug" "$@")
}

active_site() {
	[ -f "$REGISTRY" ] || return 0
	sed -n 's/^active = "\(.*\)"$/\1/p' "$REGISTRY" | head -1
}

SAVED_ACTIVE=""
restore_active() {
	local now
	now=$(active_site)
	if [ "$now" != "$SAVED_ACTIVE" ] && [ -n "$SAVED_ACTIVE" ]; then
		warn "the active site changed ($SAVED_ACTIVE -> ${now:-none}); putting $SAVED_ACTIVE back"
		NO_COLOR=1 "$BIN" sites use "$SAVED_ACTIVE" >/dev/null || warn "could not restore the active site"
	fi
}

# Registered zoo sites, in state order, then any others named zoo-*.
zoo_registered() {
	[ -f "$REGISTRY" ] || return 0
	sed -n 's/^slug = "\(zoo-.*\)"$/\1/p' "$REGISTRY"
}

is_registered() {
	zoo_registered | grep -qx "$1"
}

zoo_sites_in_order() {
	local all s
	all=$(zoo_registered)
	for s in $STATES; do
		printf '%s\n' "$all" | grep -qx "zoo-$s" && printf 'zoo-%s\n' "$s"
	done
	printf '%s\n' "$all" | while read -r s; do
		[ -n "$s" ] || continue
		case " $STATES " in *" ${s#zoo-} "*) ;; *) printf '%s\n' "$s" ;; esac
	done
}

# --- preparation ----------------------------------------------------------------

build_cli() {
	say "Building the branch's CLI (cargo build -p atelier) …"
	(cd "$REPO" && cargo build -q -p atelier) || die "cargo build failed"
	[ -x "$BIN" ] || die "no $BIN after the build"
	say "Using $BIN ($("$BIN" --version))"
}

latest_id() {
	docker image inspect --format '{{.Id}}' "$LATEST" 2>/dev/null
}

need_local_image() {
	latest_id >/dev/null || die "no local $LATEST on this computer. The zoo uses the one already here; pass --pull to let it download (docker pull $LATEST)."
}

# The atelier-cms pulls the Manager itself makes (install, update) must fetch
# nothing new unless --pull: the registry's :latest has to be the image already
# here. Offline → the pull fails without changing anything, so that passes.
guard_no_new_image() {
	if [ "$PULL" = 1 ]; then
		say "--pull: the Manager may download a newer $LATEST."
		return 0
	fi
	local remote local_digests
	remote=$(docker buildx imagetools inspect "$LATEST" 2>/dev/null | sed -n 's/^Digest: *//p' | head -1)
	if [ -z "$remote" ]; then
		warn "couldn't read $LATEST from the registry (offline?) — a pull would fail, not change anything"
		return 0
	fi
	local_digests=$(docker image inspect --format '{{range .RepoDigests}}{{.}} {{end}}' "$LATEST")
	case " $local_digests " in
	*"@$remote "*) say "Registry $LATEST = the local image ($remote): the Manager's pulls fetch nothing new." ;;
	*) die "the registry's $LATEST ($remote) is newer than the one here; the Manager's pull would download it. Pass --pull to allow that, or leave it." ;;
	esac
}

# Free memory: Docker's total minus what running containers use now (MiB).
docker_free_mem_mib() {
	local total used
	total=$(docker info --format '{{.MemTotal}}')
	used=$(docker stats --no-stream --format '{{.MemUsage}}' | awk '
		{ v=$1; n=v+0; u=v; sub(/^[0-9.]+/, "", u)
		  if (u=="GiB") n*=1024; else if (u=="KiB") n/=1024; else if (u=="B") n/=1048576
		  s+=n } END { printf "%d", s }')
	echo $((total / 1048576 - used))
}

# Docker's disk, read the way the Manager's checkup does (`df -Pk` inside a
# container, which sees the VM's disk) — in a throwaway container of a local
# image, never pulled. MiB.
docker_free_disk_mib() {
	local kb
	kb=$(docker run --rm --pull never --network none --name atelier-zoo-diskprobe \
		--entrypoint df nginx:1.30-alpine-slim -Pk / 2>/dev/null | awk 'NR==2 {print $4}')
	[ -n "$kb" ] && echo $((kb / 1024))
}

host_free_disk_mib() {
	df -Pk "$HOME/.atelier" | awk 'NR==2 {printf "%d", $4/1024}'
}

check_resources() {
	local n=$1 mem disk host need_mem need_disk floor
	need_mem=$((n * RAM_PER_SITE_MIB))
	need_disk=$((n * DISK_PER_SITE_MIB))
	floor=$((DISK_FLOOR_GIB * 1024))
	mem=$(docker_free_mem_mib)
	disk=$(docker_free_disk_mib)
	host=$(host_free_disk_mib)
	say "Estimate for $n site(s): ≈ $need_mem MiB RAM ($RAM_PER_SITE_MIB each), ≈ $need_disk MiB disk ($DISK_PER_SITE_MIB each)."
	say "Docker: $mem MiB memory free (total minus running containers), ${disk:-unknown} MiB disk free; this computer: $host MiB free."
	[ -n "$disk" ] || die "couldn't read Docker's free disk space"
	[ "$disk" -ge "$floor" ] || die "Docker has less than ${DISK_FLOOR_GIB} GiB free — refusing to add sites"
	[ "$host" -ge "$floor" ] || die "this computer has less than ${DISK_FLOOR_GIB} GiB free — refusing"
	[ "$mem" -ge "$need_mem" ] || die "Docker's free memory ($mem MiB) can't hold $n more site(s) — bring up fewer: up <state…>"
}

# --- making each state ------------------------------------------------------------

set_env() { # slug key value — replace or add one .env line
	local file
	file="$(zoo_home "$1")/.env"
	if grep -q "^$2=" "$file"; then
		sed -i '' "s|^$2=.*|$2=$3|" "$file"
	else
		printf '%s=%s\n' "$2" "$3" >>"$file"
	fi
}

add_site() { # slug — `sites add`: folder, free port, full install
	zoo_slug "$1"
	NO_COLOR=1 "$BIN" sites add "$1" --label "Zoo ${1#zoo-}" </dev/null
}

ensure_shared_tag() {
	zoo_tag "$SHARED_TAG"
	if ! docker image inspect "$SHARED_TAG" >/dev/null 2>&1; then
		# By image ID: the ghcr tag itself is never retagged or touched.
		docker tag "$(latest_id)" "$SHARED_TAG"
		say "Tagged $SHARED_TAG = $(latest_id)"
	fi
}

# Move the shared tag to another local atelier-cms image, as a release (or an
# update of one of the sites) moves a tag both sites name.
move_shared_tag() {
	local cur other
	cur=$(docker image inspect --format '{{.Id}}' "$SHARED_TAG")
	other=$(docker image ls --all --no-trunc --format '{{.ID}}' "$IMAGE" | grep -v "^$cur$" | head -1)
	if [ -z "$other" ]; then
		say "NOTE: no second local $IMAGE image here — $SHARED_TAG stays where it is (the tag-moved half of the state is missing)."
		return 0
	fi
	docker tag "$other" "$SHARED_TAG"
	say "Moved $SHARED_TAG: $cur -> $other ($(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.version"}}' "$other"))"
}

make_old_template() {
	local s=zoo-old-template home
	add_site $s || return 1
	home=$(zoo_home $s)
	atelier_on $s app stop --yes || return 1
	cp "$OLD_COMPOSE" "$home/compose.yaml"
	cp "$OLD_EDGE" "$home/edge.conf"
	# Containers created by that template, as a Manager ≤ 0.14.1 created them.
	compose_on $s up -d || return 1
	say "db restart policy: '$(docker inspect --format '{{.HostConfig.RestartPolicy.Name}}' "atelier-$s-db-1")' (the old template sets none)"
}

make_shared_tag() { # slug
	local s=$1
	ensure_shared_tag
	add_site "$s" || return 1
	set_env "$s" AINCIENT_IMAGE "$SHARED_TAG"
	compose_on "$s" up -d || return 1
}

make_shared_tag_a() {
	make_shared_tag zoo-shared-tag-a
}

make_shared_tag_b() {
	make_shared_tag zoo-shared-tag-b || return 1
	atelier_on zoo-shared-tag-b app stop --yes || return 1
	move_shared_tag
}

make_engine_resting() {
	local s=zoo-engine-resting
	if ! docker image inspect "$ENGINE_DEV_IMAGE" >/dev/null 2>&1; then
		say "SKIPPED: the branch's CLI is a developer build (no released engine digest), so \`engine enable\` pins a local $ENGINE_DEV_IMAGE, and there is none here. Building it (engine/build.sh) makes an image outside atelier-cms:zoo-*, so it is the owner's step: build it, then run \`up engine-resting\`."
		return 2
	fi
	add_site $s || return 1
	atelier_on $s engine enable --experimental || return 1
	atelier_on $s site rest --yes --experimental || return 1
}

make_stopped() {
	add_site zoo-stopped || return 1
	atelier_on zoo-stopped app stop --yes
}

make_detached_db() {
	local s=zoo-detached-db
	add_site $s || return 1
	atelier_on $s app stop --yes || return 1
	zoo_container "atelier-$s-db-1"
	zoo_container "atelier-${s}_default"
	# On a stopped container, a forced disconnect also drops the network from
	# its own settings: db then starts on no network for good (0.17.0 incident).
	docker network disconnect -f "atelier-${s}_default" "atelier-$s-db-1" || return 1
	docker start "atelier-$s-db-1" >/dev/null || return 1
	say "db networks now: '$(docker inspect --format '{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{end}}' "atelier-$s-db-1")'"
}

make_ro_backups() {
	local s=zoo-ro-backups home
	add_site $s || return 1
	home=$(zoo_home $s)
	atelier_on $s data backup --label zoo || return 1
	chmod 0555 "$home/backups"
	say "backups folder now read-only: $(ls -ld "$home/backups")"
}

make_digest_pinned() {
	local s=zoo-digest-pinned ref
	ref=$(docker image inspect --format '{{range .RepoDigests}}{{println .}}{{end}}' "$LATEST" | grep "^$IMAGE@sha256:" | head -1)
	[ -n "$ref" ] || {
		say "no registry digest for $LATEST here"
		return 1
	}
	add_site $s || return 1
	# The CLI's own way to pin an exact image.
	atelier_on $s app install --image "$ref" || return 1
}

# A running zoo site whose backup can be read (not this one, not one whose
# backups or db are broken on purpose).
pick_donor() {
	local s
	for s in zoo-digest-pinned zoo-old-template zoo-shared-tag-a; do
		is_registered $s || continue
		docker inspect --format '{{.State.Running}}' "atelier-$s-app-1" 2>/dev/null | grep -q true && {
			echo $s
			return 0
		}
	done
}

make_restored_elsewhere() {
	local s=zoo-restored-elsewhere donor file temp=0
	donor=$(pick_donor)
	if [ -z "$donor" ]; then
		donor=zoo-donor
		say "No running zoo site to borrow a backup from: making $donor for one."
		add_site $donor || return 1
		temp=1
	fi
	add_site $s || return 1
	atelier_on "$donor" data backup --label for-restored-elsewhere || return 1
	file=$(ls -t "$(zoo_home "$donor")"/backups/*.tar.gz | head -1)
	say "Restoring $donor's backup $file into $s"
	atelier_on $s data restore "$file" --yes --other-site || return 1
	if [ $temp = 1 ]; then remove_site $donor; fi
}

# --- commands ---------------------------------------------------------------------

cmd_up() {
	local want="$*" s fn rc made="" skipped="" failed="" n=0 log
	[ -n "$want" ] || want=$STATES
	for s in $want; do
		case " $STATES " in *" $s "*) ;; *) die "unknown state '$s' (states: $STATES)" ;; esac
		is_registered "zoo-$s" || n=$((n + 1))
	done
	build_cli
	need_local_image
	guard_no_new_image
	[ $n -gt 0 ] && check_resources $n
	mkdir -p "$LOGS"
	for s in $want; do
		if is_registered "zoo-$s"; then
			say "== $s: zoo-$s is already up"
			continue
		fi
		fn="make_$(printf '%s' "$s" | tr - _)"
		log="$LOGS/$s-up.log"
		say "== $s: making zoo-$s (log: $log)"
		local t0=$SECONDS
		$fn >"$log" 2>&1
		rc=$?
		case $rc in
		0) made="$made $s" ;;
		2) skipped="$skipped $s" ;;
		*) failed="$failed $s" ;;
		esac
		say "   exit $rc after $((SECONDS - t0)) s — $(last_line "$log")"
	done
	say ""
	say "Made:   ${made:- none}"
	say "Skipped:${skipped:- none}"
	say "Failed: ${failed:- none}"
	[ -z "$failed" ]
}

# Take one zoo site away completely: containers, volumes, network, folder.
remove_site() {
	local s=$1 home
	home=$(zoo_home "$s") || return 1
	if is_registered "$s"; then
		atelier_on "$s" app down --wipe --yes || warn "$s: app down --wipe failed; removing what's left by hand"
		NO_COLOR=1 "$BIN" sites remove "$s" >/dev/null || warn "$s: sites remove failed"
	fi
	# Anything the project still owns (an engine site's extra containers, a
	# failed down): by the compose project label, each name checked.
	local p="atelier-$s" c
	zoo_project "$p"
	for c in $(docker ps -aq --filter "label=com.docker.compose.project=$p"); do
		zoo_container "$(docker inspect --format '{{.Name}}' "$c" | sed 's|^/||')"
		docker rm -f "$c" >/dev/null
	done
	for c in $(docker volume ls -q --filter "label=com.docker.compose.project=$p"); do
		zoo_container "$c"
		docker volume rm "$c" >/dev/null
	done
	for c in $(docker network ls -q --filter "label=com.docker.compose.project=$p"); do
		zoo_container "$(docker network inspect --format '{{.Name}}' "$c")"
		docker network rm "$c" >/dev/null
	done
	case "$home" in
	"$HOME/.atelier/sites/zoo-"?*)
		if [ -d "$home" ]; then
			chmod -R u+w "$home"
			rm -rf "$home"
		fi
		;;
	*) die "refused to delete $home" ;;
	esac
	say "removed $s"
}

cmd_down() {
	local s list tag
	build_cli
	if [ $# -gt 0 ]; then
		# Some states only: their sites go, the shared tag stays.
		for s in "$@"; do remove_site "zoo-$s"; done
		return 0
	fi
	list=$(
		zoo_registered
		for s in "$SITES_DIR"/zoo-*; do [ -d "$s" ] && basename "$s"; done
	)
	for s in $(printf '%s\n' "$list" | sort -u); do
		remove_site "$s"
	done
	for tag in $(docker image ls --format '{{.Repository}}:{{.Tag}}' atelier-cms | grep '^atelier-cms:zoo-'); do
		zoo_tag "$tag"
		docker image rm "$tag" >/dev/null && say "untagged $tag"
	done
	say "Zoo is empty."
}

cmd_status() {
	local s home
	build_cli >/dev/null
	say "Sites (atelier sites list, zoo only):"
	NO_COLOR=1 "$BIN" sites list | grep -E '^ *\*? *zoo-|SITE' || say "  (none)"
	say ""
	for s in $(zoo_sites_in_order); do
		home=$(zoo_home "$s")
		say "$s  image=$(sed -n 's/^AINCIENT_IMAGE=//p' "$home/.env" 2>/dev/null)"
		docker ps -a --filter "label=com.docker.compose.project=atelier-$s" \
			--format '    {{.Names}}  {{.State}}  {{.Status}}  {{.Image}}'
	done
}

# The last line worth reading in a log, ANSI colour removed.
last_line() {
	sed -e "s/$(printf '\033')\\[[0-9;]*m//g" "$1" | grep -v '^[[:space:]]*$' | tail -1 | cut -c1-110
}

cmd_check() {
	[ $# -gt 0 ] || die "check needs an atelier command, e.g. check app start"
	local name s log rc t0 dt out sites
	build_cli
	guard_no_new_image
	sites=$(zoo_sites_in_order)
	if [ -n "${ZOO_ONLY:-}" ]; then
		sites=$(for s in $ZOO_ONLY; do printf '%s\n' "$sites" | grep -x "zoo-$s"; done)
	fi
	[ -n "$sites" ] || die "no zoo sites — run up first"
	mkdir -p "$LOGS"
	name=$(printf '%s' "$*" | tr -c 'A-Za-z0-9' - | sed 's/--*/-/g; s/-$//')
	out="$LOGS/check-$name.tsv"
	: >"$out"
	for s in $sites; do
		log="$LOGS/${s#zoo-}-$name.log"
		say "-- ${s#zoo-}: atelier --site $s $*"
		t0=$SECONDS
		atelier_on "$s" "$@" >"$log" 2>&1
		rc=$?
		dt=$((SECONDS - t0))
		printf '%s\t%s\t%s\t%s\n' "${s#zoo-}" "$rc" "$(last_line "$log")" "${dt}s" >>"$out"
	done
	say ""
	say "atelier $*"
	printf '%-20s %-5s %-6s %s\n' STATE EXIT TIME OUTCOME
	while IFS="$(printf '\t')" read -r st rc line dt; do
		printf '%-20s %-5s %-6s %s\n' "$st" "$rc" "$dt" "$line"
	done <"$out"
	say ""
	say "Full output: $LOGS/<state>-$name.log"
}

usage() {
	sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'
	exit 2
}

main() {
	unset ATELIER_HOME
	command -v docker >/dev/null || die "docker not found"
	docker info >/dev/null 2>&1 || die "Docker isn't answering — start it yourself; the zoo never starts or restarts it"
	while [ $# -gt 0 ]; do
		case "$1" in
		--pull) PULL=1 ;;
		-h | --help) usage ;;
		*) break ;;
		esac
		shift
	done
	[ $# -gt 0 ] || usage
	SAVED_ACTIVE=$(active_site)
	trap restore_active EXIT
	local cmd=$1
	shift
	case "$cmd" in
	up) cmd_up "$@" ;;
	down) cmd_down "$@" ;;
	status) cmd_status ;;
	check) cmd_check "$@" ;;
	*) usage ;;
	esac
}

main "$@"
