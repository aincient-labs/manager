//! The API's rules, called the way edge calls it, against a fake site.

use super::*;
use crate::admin::{self, AdminCommand};
use crate::site::Cut;
use crate::state::tests::TempDir;
use std::sync::atomic::{AtomicBool, Ordering};

/// The site, faked: records every call; `app` and `db` are up or not.
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<String>>,
    up: AtomicBool,
    /// drush never returns on its own (a compromised Drupal), only when cut.
    drush_hangs: AtomicBool,
}

impl Fake {
    fn calls(&self) -> Vec<String> {
        lock(&self.calls).clone()
    }
    fn record(&self, s: &str) {
        lock(&self.calls).push(s.to_string());
    }
}

struct Shared(Arc<Fake>);

impl Site for Shared {
    fn states(&self) -> Result<Value> {
        let s = if self.0.up.load(Ordering::SeqCst) {
            "running"
        } else {
            "exited"
        };
        Ok(json!([{"Service": "app", "State": s}, {"Service": "db", "State": s}]))
    }
    fn stop_app_and_db(&self, _cut: Cut) -> Result<()> {
        self.0.record("stop");
        self.0.up.store(false, Ordering::SeqCst);
        Ok(())
    }
    fn start_db_and_app(&self, _cut: Cut) -> Result<()> {
        self.0.record("start");
        self.0.up.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn drush(&self, verb: Drush, cut: Cut) -> Result<()> {
        self.0.record(&format!("drush {verb:?}"));
        let t0 = Instant::now();
        while self.0.drush_hangs.load(Ordering::SeqCst) {
            if cut() {
                bail!("cut short by a lockdown");
            }
            assert!(t0.elapsed() < Duration::from_secs(10), "never cut");
            thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }
    fn answers(&self) -> bool {
        self.0.up.load(Ordering::SeqCst)
    }
}

struct T {
    e: Arc<Engine>,
    fake: Arc<Fake>,
    d: TempDir,
}

const OWNER_IP: &str = "192.0.2.10";

fn engine() -> T {
    let d = TempDir::new();
    let fake = Arc::new(Fake::default());
    fake.up.store(true, Ordering::SeqCst);
    let store = Store::at(d.0.join("state")).unwrap();
    let inbox = Inbox::at(d.0.join("inbox"), d.0.join("outbox"));
    std::fs::create_dir_all(&inbox.dir).unwrap();
    std::fs::create_dir_all(&inbox.out).unwrap();
    let e = Engine::new(
        store,
        inbox,
        Box::new(Shared(Arc::clone(&fake))),
        "127.0.0.1:80",
    )
    .unwrap();
    T {
        e: Arc::new(e),
        fake,
        d,
    }
}

impl T {
    fn req(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> (u16, Value) {
        let r = self.e.handle(Request {
            method: method.into(),
            url: path.into(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.as_bytes().to_vec(),
            peer: "172.20.0.3:5000".parse().unwrap(),
        });
        let v = serde_json::from_str(&r.body).unwrap_or(Value::String(r.body));
        (r.status, v)
    }

    /// A call through edge from `ip`, with a token or none.
    fn call(&self, method: &str, path: &str, token: Option<&str>, body: &str) -> (u16, Value) {
        self.call_from(OWNER_IP, method, path, token, body)
    }

    fn call_from(
        &self,
        ip: &str,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: &str,
    ) -> (u16, Value) {
        let auth = token.map(|t| format!("Bearer {t}"));
        let mut h = vec![("X-Real-IP", ip)];
        if let Some(a) = &auth {
            h.push(("Authorization", a));
        }
        self.req(method, path, &h, body)
    }

    fn admin(&self, cmd: AdminCommand, stdin: &str) -> String {
        let mut out = Vec::new();
        admin::run(
            cmd,
            &self.e.store,
            &self.e.inbox,
            &Shared(self.fake.clone()),
            &mut stdin.as_bytes(),
            &mut out,
        )
        .unwrap();
        String::from_utf8(out).unwrap().trim().to_string()
    }

    /// The owner's Manager, paired with the install code: `(id, token)`.
    fn primary(&self) -> (String, String) {
        let code = self.admin(AdminCommand::InstallCode, "");
        let (s, v) = self.call(
            "POST",
            "/v1/pair",
            None,
            &json!({"code": code, "name": "Owner"}).to_string(),
        );
        assert_eq!(s, 201, "{v}");
        (s_(&v["id"]), s_(&v["token"]))
    }

    /// Another Manager, by the code swap the primary approves.
    fn junior(&self, primary: &str, ip: &str) -> (String, String) {
        let (s, v) = self.call_from(ip, "POST", "/v1/pair/request", None, r#"{"name":"Laptop"}"#);
        assert_eq!(s, 201, "{v}");
        let (code, handle) = (s_(&v["code"]), s_(&v["handle"]));
        let (s, _) = self.call(
            "POST",
            "/v1/pairings/approve",
            Some(primary),
            &json!({ "code": code }).to_string(),
        );
        assert_eq!(s, 200);
        let (s, v) = self.call_from(
            ip,
            "POST",
            "/v1/pair/collect",
            None,
            &json!({ "handle": handle }).to_string(),
        );
        assert_eq!(s, 201, "{v}");
        (s_(&v["id"]), s_(&v["token"]))
    }

    /// Wait for the running job to finish; returns its status entry.
    fn job_done(&self, token: &str) -> Value {
        let t0 = Instant::now();
        loop {
            let (_, v) = self.call("GET", "/v1/status", Some(token), "");
            if v["job"]["finished"].is_u64() {
                return v["job"].clone();
            }
            assert!(
                t0.elapsed() < Duration::from_secs(10),
                "job never finished: {v}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn audit_text(&self) -> String {
        std::fs::read_to_string(self.d.0.join("state/audit.log")).unwrap_or_default()
    }
}

fn s_(v: &Value) -> String {
    v.as_str().unwrap().to_string()
}

#[test]
fn only_listed_paths_route() {
    assert_eq!(route("GET", "/v1/version"), Route::Version);
    assert_eq!(route("GET", "/v1/version?x=1"), Route::Version);
    assert_eq!(route("POST", "/v1/version"), Route::MethodNotAllowed);
    assert_eq!(route("GET", "/v1/wake"), Route::WakePage);
    assert_eq!(route("POST", "/v1/wake"), Route::Wake);
    assert_eq!(route("PUT", "/v1/wake"), Route::MethodNotAllowed);
    assert_eq!(route("GET", "/v1/rest"), Route::MethodNotAllowed);
    assert_eq!(
        route("DELETE", "/v1/pairings/approve"),
        Route::MethodNotAllowed
    );
    assert_eq!(
        route("DELETE", "/v1/pairings/abcdefghjkmn"),
        Route::Revoke("abcdefghjkmn".into())
    );
    assert_eq!(
        route("GET", "/v1/pairings/abcdefghjkmn"),
        Route::MethodNotAllowed
    );
    for bad in [
        "/v1/../v1/status",
        "/v1//status",
        "/v1/status/",
        "/v1/%73tatus",
        "/V1/status",
        "/v2/status",
        "/status",
        "/v1/exec",
        "/v1/pairings/..%2f..%2fx",
        "/v1/pairings/abcdefghjkmn/x",
        "/v1/pairings/ABCDEFGHJKMN",
        "",
    ] {
        assert_eq!(route("GET", bad), Route::NotFound, "{bad}");
        assert_eq!(route("DELETE", bad), Route::NotFound, "{bad}");
    }
}

/// Every engine path is one edge routes, and every route edge names is one
/// the engine has: the two lists can't drift.
#[test]
fn edge_names_exactly_the_engine_routes() {
    let mut edge: Vec<&str> = aincient_core::engine::EDGE_ROUTES.to_vec();
    edge.extend(["pair", "wake"]);
    edge.sort();
    let mut ours = PATHS.to_vec();
    ours.sort();
    assert_eq!(edge, ours);
    let conf = aincient_core::engine::edge_conf();
    for p in PATHS {
        let concrete = p.replace("[0-9a-z]{12}", "abcdefghjkmn");
        assert!(
            ["GET", "POST", "DELETE"]
                .iter()
                .any(|m| route(m, &format!("/v1/{concrete}")) != Route::NotFound),
            "{p}"
        );
        assert!(conf.contains(p), "edge lacks {p}");
    }
}

#[test]
fn unknown_paths_are_404_and_wrong_methods_405() {
    let t = engine();
    let (_, tok) = t.primary();
    assert_eq!(t.call("GET", "/v1/exec", Some(&tok), "").0, 404);
    assert_eq!(t.call("GET", "/v1/exec", None, "").0, 404);
    assert_eq!(
        t.call("DELETE", "/v1/audit", Some(&tok), "").0,
        405,
        "nothing truncates the log"
    );
    assert_eq!(t.call("GET", "/v1/lockdown", Some(&tok), "").0, 405);
    assert_eq!(t.call("POST", "/v1/version", None, "").0, 405);
}

/// The auth rule of every route: none, install code, handle, wake code,
/// token, primary token.
#[test]
fn every_route_has_its_auth_rule() {
    let t = engine();
    let (_, owner) = t.primary();
    let (_, junior) = t.junior(&owner, "192.0.2.20");

    // None.
    let (s, v) = t.call("GET", "/v1/version", None, "");
    assert_eq!(s, 200);
    assert_eq!(v["api"], API_VERSION);
    assert_eq!(v["engine"], ENGINE_VERSION);
    assert_eq!(t.call("GET", "/v1/wake", None, "").0, 200);
    assert_eq!(
        t.call_from(
            "192.0.2.30",
            "POST",
            "/v1/pair/request",
            None,
            r#"{"name":"x"}"#
        )
        .0,
        201
    );
    // Install code.
    assert_eq!(
        t.call(
            "POST",
            "/v1/pair",
            None,
            r#"{"code":"0000-0000-0000-0000","name":"x"}"#
        )
        .0,
        401
    );
    // Handle.
    let h = codes::new_handle();
    assert_eq!(
        t.call(
            "POST",
            "/v1/pair/collect",
            None,
            &json!({ "handle": h }).to_string()
        )
        .0,
        404
    );
    // Wake code.
    assert_eq!(
        t.call("POST", "/v1/wake", None, r#"{"code":"wrong wrong wrong"}"#)
            .0,
        401
    );

    // Token: none, junk, made-up, then a junior's works.
    let token_routes = [
        ("GET", "/v1/status"),
        ("POST", "/v1/wake/paired"),
        ("POST", "/v1/freeze"),
        ("POST", "/v1/live"),
        ("POST", "/v1/rest"),
        ("POST", "/v1/lockdown"),
        ("POST", "/v1/unlock"),
        ("GET", "/v1/pairings"),
        ("DELETE", "/v1/pairings/self"),
        ("DELETE", "/v1/pairings/abcdefghjkmn"),
        ("GET", "/v1/audit"),
        ("GET", "/v1/pairings/requests"),
        ("POST", "/v1/pairings/approve"),
        ("POST", "/v1/wake/resume"),
    ];
    for (m, p) in token_routes {
        assert_eq!(t.call(m, p, None, "").0, 401, "{m} {p} without a token");
        assert_eq!(t.call(m, p, Some("junk"), "").0, 401, "{m} {p} junk");
        assert_eq!(
            t.call(m, p, Some(&codes::new_token()), "").0,
            401,
            "{m} {p} made-up"
        );
    }
    let (s, v) = t.req(
        "GET",
        "/v1/status",
        &[("Cookie", "SESSabc=1"), ("X-Real-IP", OWNER_IP)],
        "",
    );
    assert_eq!(s, 401, "a session cookie authenticates nothing: {v}");
    assert_eq!(t.call("GET", "/v1/status", Some(&junior), "").0, 200);
    assert_eq!(t.call("GET", "/v1/pairings", Some(&junior), "").0, 200);
    assert_eq!(t.call("GET", "/v1/audit", Some(&junior), "").0, 200);

    // Primary token.
    for (m, p, b) in [
        ("GET", "/v1/pairings/requests", ""),
        ("POST", "/v1/pairings/approve", r#"{"code":"ABCD-EFGH"}"#),
        ("POST", "/v1/wake/resume", ""),
    ] {
        assert_eq!(t.call(m, p, Some(&junior), b).0, 403, "{p} by a junior");
    }
    assert_eq!(
        t.call("GET", "/v1/pairings/requests", Some(&owner), "").0,
        200
    );
    assert_eq!(t.call("POST", "/v1/wake/resume", Some(&owner), "").0, 200);
    assert_eq!(
        t.call(
            "POST",
            "/v1/pairings/approve",
            Some(&owner),
            r#"{"code":"ABCD-EFGH"}"#
        )
        .0,
        404
    );
}

#[test]
fn bodies_and_fields_are_closed_and_capped() {
    let t = engine();
    let (_, owner) = t.primary();
    for (path, body) in [
        ("/v1/pair", r#"{"code":"x"}"#),
        ("/v1/pair", r#"{"code":"x","name":"y","primary":true}"#),
        ("/v1/pair", "not json"),
        ("/v1/pair/request", r#"{"name":"x","code":"y"}"#),
        ("/v1/pair/request", r#"{"name":""}"#),
        ("/v1/pair/request", "{\"name\":\"a\\u202eb\"}"),
        ("/v1/pair/collect", r#"{"handle":"00"}"#),
        ("/v1/pair/collect", r#"{"handle":"../../state"}"#),
        ("/v1/wake", r#"{"code":"x","name":"y"}"#),
    ] {
        assert_eq!(t.call("POST", path, None, body).0, 400, "{path} {body}");
    }
    let long = "A".repeat(codes::CODE_FIELD_MAX + 1);
    let name65 = "x".repeat(65);
    assert_eq!(
        t.call(
            "POST",
            "/v1/pair",
            None,
            &json!({"code": long, "name": "x"}).to_string()
        )
        .0,
        400
    );
    assert_eq!(
        t.call(
            "POST",
            "/v1/pair/request",
            None,
            &json!({ "name": name65 }).to_string()
        )
        .0,
        400
    );
    assert_eq!(
        t.call(
            "POST",
            "/v1/wake",
            None,
            &json!({ "code": long }).to_string()
        )
        .0,
        400
    );
    assert_eq!(
        t.call(
            "POST",
            "/v1/pairings/approve",
            Some(&owner),
            &json!({ "code": long }).to_string()
        )
        .0,
        400
    );
    assert_eq!(
        t.call(
            "POST",
            "/v1/pairings/approve",
            Some(&owner),
            r#"{"code":"x","id":"y"}"#
        )
        .0,
        400
    );
    // A name is trimmed, then checked; bidi in the middle is refused.
    let (s, v) = t.call_from(
        "192.0.2.40",
        "POST",
        "/v1/pair/request",
        None,
        r#"{"name":"  Spaced  "}"#,
    );
    assert_eq!(s, 201, "{v}");
    let (_, v) = t.call("GET", "/v1/pairings/requests", Some(&owner), "");
    assert_eq!(v["requests"][0]["name"], "Spaced");
}

#[test]
fn approve_by_code_lets_in_only_the_request_shown() {
    let t = engine();
    let (owner_id, owner) = t.primary();
    let (_, v) = t.call_from(
        "192.0.2.50",
        "POST",
        "/v1/pair/request",
        None,
        r#"{"name":"New"}"#,
    );
    let (code, handle) = (s_(&v["code"]), s_(&v["handle"]));
    assert_eq!(code.len(), 9, "XXXX-XXXX");
    let collect = || {
        t.call_from(
            "192.0.2.50",
            "POST",
            "/v1/pair/collect",
            None,
            &json!({ "handle": handle }).to_string(),
        )
    };
    assert_eq!(collect().0, 202, "no key before approval");
    let (_, list) = t.call("GET", "/v1/pairings/requests", Some(&owner), "");
    let text = list.to_string();
    assert!(text.contains("New") && text.contains("192.0.2.50"));
    assert!(
        !text.contains(&code) && !text.contains(&code.replace('-', "")),
        "never the code"
    );
    let (s, _) = t.call(
        "POST",
        "/v1/pairings/approve",
        Some(&owner),
        &json!({ "code": code.to_lowercase() }).to_string(),
    );
    assert_eq!(s, 200, "typed lower case");
    let (s, v) = collect();
    assert_eq!(s, 201);
    assert!(codes::is_token_shaped(&s_(&v["token"])));
    assert_eq!(collect().0, 404, "collected once");
    let (_, v) = t.call("GET", "/v1/pairings", Some(&owner), "");
    let all = v["pairings"].as_array().unwrap();
    assert_eq!(all.len(), 2);
    assert!(all.iter().all(|p| p.get("token_sha256").is_none()));
    let new = all.iter().find(|p| p["name"] == "New").unwrap();
    assert_eq!(new["primary"], false);
    assert_eq!(new["via"], format!("approved by {owner_id}"));
    // SSH approves too.
    let (_, v) = t.call_from(
        "192.0.2.51",
        "POST",
        "/v1/pair/request",
        None,
        r#"{"name":"Ssh"}"#,
    );
    t.admin(
        AdminCommand::Approve {
            code: s_(&v["code"]),
        },
        "",
    );
    let (s, _) = t.call_from(
        "192.0.2.51",
        "POST",
        "/v1/pair/collect",
        None,
        &json!({ "handle": v["handle"] }).to_string(),
    );
    assert_eq!(s, 201);
    let audit = t.audit_text();
    assert!(audit.contains("pairings/approve") && audit.contains("pair/request"));
}

#[test]
fn seniority_rules_hold_through_the_api() {
    let t = engine();
    let (owner_id, owner) = t.primary();
    let (second_id, second) = t.junior(&owner, "192.0.2.20");
    let (third_id, third) = t.junior(&owner, "192.0.2.21");
    let del = |id: &str, tok: &str| {
        t.call("DELETE", &format!("/v1/pairings/{id}"), Some(tok), "")
            .0
    };
    assert_eq!(
        del(&owner_id, &second),
        403,
        "a junior can't revoke the owner"
    );
    assert_eq!(del(&second_id, &third), 403, "nor one senior to it");
    assert_eq!(del("abcdefghjkmn", &second), 404);
    assert_eq!(del(&third_id, &second), 200, "a senior revokes a junior");
    assert_eq!(
        t.call("GET", "/v1/status", Some(&third), "").0,
        401,
        "revoked fails next call"
    );
    assert_eq!(
        t.call("DELETE", "/v1/pairings/self", Some(&second), "").0,
        200
    );
    assert_eq!(t.call("GET", "/v1/status", Some(&second), "").0, 401);
    // The console (the inbox) revokes anything but the primary.
    let (fourth_id, _) = t.junior(&owner, "192.0.2.22");
    for id in [&owner_id, &fourth_id] {
        std::fs::write(
            t.e.inbox.dir.join("revoke.request"),
            json!({ "id": id }).to_string(),
        )
        .unwrap();
        t.e.tick(1);
    }
    let (s, v) = t.call("GET", "/v1/pairings", Some(&owner), "");
    assert_eq!(s, 200, "the console couldn't revoke the primary");
    assert_eq!(v["pairings"].as_array().unwrap().len(), 1);
    let published = std::fs::read_to_string(t.e.inbox.out.join("pairings.json")).unwrap();
    assert!(published.contains(&owner_id) && !published.contains(&fourth_id));
    assert!(t.audit_text().contains("console (its claim)"));
}

#[test]
fn lockdown_revokes_juniors_refuses_verbs_and_closes_pairing() {
    let t = engine();
    let (owner_id, owner) = t.primary();
    let (second_id, second) = t.junior(&owner, "192.0.2.20");
    let (_, third) = t.junior(&owner, "192.0.2.21");
    let (_, v) = t.call_from(
        "192.0.2.60",
        "POST",
        "/v1/pair/request",
        None,
        r#"{"name":"Waiting"}"#,
    );
    let handle = s_(&v["handle"]);
    t.call(
        "POST",
        "/v1/pairings/approve",
        Some(&owner),
        &json!({"code": v["code"]}).to_string(),
    );
    t.admin(
        AdminCommand::WakeCode { set: true },
        "the owner's wake code",
    );
    let code = t.admin(AdminCommand::InstallCode, "");

    // The second Manager locks down: the third (junior to it) goes.
    let (s, v) = t.call("POST", "/v1/lockdown", Some(&second), "");
    assert_eq!(s, 202, "{v}");
    let job = t.job_done(&second);
    assert_eq!(job["verb"], "lockdown");
    assert!(job["error"].is_null(), "{job}");
    assert_eq!(t.fake.calls(), ["stop"], "stopped, nothing of Drupal's ran");
    assert_eq!(t.call("GET", "/v1/status", Some(&third), "").0, 401);
    assert_eq!(
        t.call("GET", "/v1/status", Some(&owner), "").0,
        200,
        "the senior stays"
    );

    // Refused while locked down.
    for (m, p, b) in [
        ("POST", "/v1/freeze", ""),
        ("POST", "/v1/live", ""),
        ("POST", "/v1/rest", ""),
        ("POST", "/v1/wake/paired", ""),
        ("POST", "/v1/wake/resume", ""),
        ("GET", "/v1/pairings/requests", ""),
        ("POST", "/v1/pairings/approve", r#"{"code":"ABCD-EFGH"}"#),
    ] {
        assert_eq!(t.call(m, p, Some(&owner), b).0, 409, "{p}");
    }
    assert_eq!(
        t.call("POST", "/v1/pair/request", None, r#"{"name":"x"}"#)
            .0,
        403
    );
    assert_eq!(
        t.call_from(
            "192.0.2.60",
            "POST",
            "/v1/pair/collect",
            None,
            &json!({ "handle": handle }).to_string()
        )
        .0,
        404,
        "approved before the lockdown: no key"
    );
    assert_eq!(
        t.call(
            "POST",
            "/v1/pair",
            None,
            &json!({"code": code, "name": "x"}).to_string()
        )
        .0,
        403,
        "even the install code can't pair"
    );
    assert_eq!(
        t.call(
            "POST",
            "/v1/wake",
            None,
            r#"{"code":"the owner's wake code"}"#
        )
        .0,
        403,
        "the right wake code is refused"
    );
    // Allowed while locked down.
    for (m, p) in [
        ("GET", "/v1/status"),
        ("GET", "/v1/pairings"),
        ("GET", "/v1/audit"),
    ] {
        assert_eq!(t.call(m, p, Some(&owner), "").0, 200, "{p}");
    }
    assert_eq!(
        t.call("POST", "/v1/lockdown", Some(&owner), "").0,
        202,
        "never refused"
    );
    t.job_done(&owner);
    assert_eq!(
        t.call("GET", "/v1/status", Some(&second), "").0,
        401,
        "the owner's lockdown takes the second"
    );

    // A site started by hand during a lockdown is stopped again.
    t.fake.up.store(true, Ordering::SeqCst);
    t.e.tick(0);
    t.job_done(&owner);
    assert!(!t.fake.up.load(Ordering::SeqCst));

    assert_eq!(t.call("POST", "/v1/unlock", Some(&owner), "").0, 200);
    assert_eq!(t.call("POST", "/v1/wake/paired", Some(&owner), "").0, 202);
    let job = t.job_done(&owner);
    assert!(job["error"].is_null(), "{job}");
    assert!(t.fake.up.load(Ordering::SeqCst));
    let a = t.audit_text();
    assert!(a.contains(&second_id) || a.contains("revoked junior pairings"));
    assert!(a.contains(&owner_id));
}

#[test]
fn a_lockdown_cuts_a_hung_drush_short_and_still_stops_the_site() {
    let t = engine();
    let (_, owner) = t.primary();
    t.fake.drush_hangs.store(true, Ordering::SeqCst);
    assert_eq!(t.call("POST", "/v1/freeze", Some(&owner), "").0, 202);
    assert_eq!(
        t.call("POST", "/v1/rest", Some(&owner), "").0,
        409,
        "one job at a time"
    );
    thread::sleep(Duration::from_millis(100));
    let (s, v) = t.call("POST", "/v1/lockdown", Some(&owner), "");
    // Queued behind the freeze it cuts short, or (if the cut won the race)
    // started on its own: never refused.
    assert_eq!(s, 202, "{v}");
    let job = t.job_done(&owner);
    assert_eq!(job["verb"], "lockdown");
    assert!(job["error"].is_null(), "{job}");
    let calls = t.fake.calls();
    assert_eq!(calls[0], "drush Freeze");
    assert!(
        calls[1..].iter().all(|c| c == "stop") && calls.len() >= 2,
        "{calls:?}"
    );
    assert!(!t.fake.up.load(Ordering::SeqCst));
    assert!(t.audit_text().contains("cut short by a lockdown"));
}

#[test]
fn rest_freezes_first_and_a_failed_freeze_keeps_the_site_awake() {
    struct Failing;
    impl Site for Failing {
        fn states(&self) -> Result<Value> {
            Ok(json!([{"Service": "app", "State": "running"}]))
        }
        fn stop_app_and_db(&self, _: Cut) -> Result<()> {
            panic!("must not stop after a failed freeze")
        }
        fn start_db_and_app(&self, _: Cut) -> Result<()> {
            Ok(())
        }
        fn drush(&self, _: Drush, _: Cut) -> Result<()> {
            bail!("drush said no")
        }
        fn answers(&self) -> bool {
            true
        }
    }
    let d = TempDir::new();
    let e = Arc::new(
        Engine::new(
            Store::at(d.0.join("s")).unwrap(),
            Inbox::at(d.0.join("i"), d.0.join("o")),
            Box::new(Failing),
            "127.0.0.1:80",
        )
        .unwrap(),
    );
    let t = T {
        e,
        fake: Arc::new(Fake::default()),
        d,
    };
    let (_, owner) = t.primary();
    assert_eq!(t.call("POST", "/v1/rest", Some(&owner), "").0, 202);
    let job = t.job_done(&owner);
    let err = s_(&job["error"]);
    assert!(
        err.contains("stays awake") && err.contains("drush said no"),
        "{err}"
    );

    let t2 = engine();
    let (_, owner) = t2.primary();
    assert_eq!(t2.call("POST", "/v1/rest", Some(&owner), "").0, 202);
    t2.job_done(&owner);
    assert_eq!(t2.fake.calls(), ["drush Freeze", "stop"]);
}

#[test]
fn wake_by_code_pauses_per_address_and_then_for_everyone() {
    let t = engine();
    let (_, owner) = t.primary();
    t.fake.up.store(false, Ordering::SeqCst);
    let wake = t.admin(AdminCommand::WakeCode { set: false }, "");
    let try_from = |ip: &str, code: &str| {
        t.call_from(
            ip,
            "POST",
            "/v1/wake",
            None,
            &json!({ "code": code }).to_string(),
        )
        .0
    };
    let (s1, b1) = t.call_from("198.51.100.1", "POST", "/v1/wake", None, r#"{"code":"x"}"#);
    let (s2, b2) = t.call_from(
        "198.51.100.1",
        "POST",
        "/v1/wake",
        None,
        r#"{"code":"a-much-longer-wrong-guess"}"#,
    );
    assert_eq!(
        (s1, &b1),
        (s2, &b2),
        "every wrong code gets the same answer"
    );
    for _ in 2..state::WAKE_TRIES_PER_IP {
        assert_eq!(try_from("198.51.100.1", "wrong wrong wrong"), 401);
    }
    assert_eq!(
        try_from("198.51.100.1", &wake),
        429,
        "this address is paused, right code or not"
    );
    assert_eq!(
        try_from("203.0.113.7", "wrong wrong wrong"),
        401,
        "another address isn't"
    );
    // Many addresses: everyone pauses.
    {
        let l = t.e.store.lock().unwrap();
        let at = now();
        l.update_flags(|f| {
            for i in 0..state::WAKE_TRIES_PER_HOUR {
                f.wake_try(&format!("10.1.{}.{}", i / 250, i % 250), at);
            }
        })
        .unwrap();
    }
    assert_eq!(try_from(OWNER_IP, &wake), 429);
    let (_, st) = t.call("GET", "/v1/status", Some(&owner), "");
    assert_eq!(st["wake_paused"], true);
    assert!(st["wake_paused_until"].as_u64().unwrap() > now() + 3000);
    // The primary lifts it; the right code then wakes, and only wakes.
    assert_eq!(t.call("POST", "/v1/wake/resume", Some(&owner), "").0, 200);
    let (s, v) = t.call(
        "POST",
        "/v1/wake",
        None,
        &json!({ "code": wake.to_lowercase() }).to_string(),
    );
    assert_eq!(s, 202, "{v}");
    assert!(v.get("token").is_none(), "waking signs nobody in");
    t.job_done(&owner);
    assert_eq!(t.fake.calls(), ["start"]);
    assert!(t.fake.up.load(Ordering::SeqCst));
}

#[test]
fn the_wake_form_answers_with_the_page_and_never_echoes_input() {
    let t = engine();
    t.admin(AdminCommand::WakeCode { set: false }, "");
    let r = t.e.handle(Request {
        method: "POST".into(),
        url: "/v1/wake".into(),
        headers: vec![(
            "Content-Type".into(),
            "application/x-www-form-urlencoded".into(),
        )],
        body: b"code=%3Cscript%3Ealert(1)%3C%2Fscript%3E".to_vec(),
        peer: "172.20.0.3:5000".parse().unwrap(),
    });
    assert_eq!(r.status, 401);
    assert!(r.content_type.starts_with("text/html"));
    assert!(!r.body.contains("<script>"));
    assert!(r.body.contains("That code didn't work."));
    assert!(r
        .headers
        .iter()
        .any(|(k, v)| *k == "Content-Security-Policy" && v.contains("frame-ancestors 'none'")));
    assert_eq!(
        form_field(b"a=1&code=ab%2Dcd+ef", "code").unwrap(),
        "ab-cd ef"
    );
    assert_eq!(form_field(b"code=%zz%4", "code").unwrap(), "%zz%4");
    assert!(form_field(b"other=1", "code").is_none());
}

#[test]
fn made_up_tokens_from_one_address_are_told_to_back_off_but_a_real_one_gets_in() {
    let t = engine();
    let (_, owner) = t.primary();
    let mut last = 0;
    for _ in 0..AUTH_FAILS_PER_MINUTE + 5 {
        last = t
            .call_from(
                "198.51.100.9",
                "GET",
                "/v1/status",
                Some(&codes::new_token()),
                "",
            )
            .0;
    }
    assert_eq!(last, 429);
    assert_eq!(
        t.call_from("198.51.100.9", "GET", "/v1/status", Some(&owner), "")
            .0,
        200
    );
    let lines = t
        .audit_text()
        .lines()
        .filter(|l| l.contains("198.51.100.9"))
        .count();
    assert!(lines <= 3, "refusals collapse in the audit log: {lines}");
}

#[test]
fn a_junk_real_ip_header_falls_back_to_the_peer() {
    let t = engine();
    for _ in 0..state::REQUESTS_PER_IP {
        let (s, _) = t.req(
            "POST",
            "/v1/pair/request",
            &[("X-Real-IP", "not-an-ip")],
            r#"{"name":"x"}"#,
        );
        assert_eq!(s, 201);
    }
    let (s, _) = t.req(
        "POST",
        "/v1/pair/request",
        &[("X-Real-IP", "also junk")],
        r#"{"name":"x"}"#,
    );
    assert_eq!(
        s, 429,
        "both counted against the peer, not two made-up keys"
    );
}

#[test]
fn only_edge_may_connect() {
    let t = engine();
    assert!(t.e.is_edge(&"127.0.0.1:4000".parse().unwrap()));
    assert!(!t.e.is_edge(&"10.0.0.9:4000".parse().unwrap()));

    // Over a real socket: an engine whose edge is elsewhere closes us unread.
    let d = TempDir::new();
    let far = Arc::new(
        Engine::new(
            Store::at(d.0.join("s")).unwrap(),
            Inbox::at(d.0.join("i"), d.0.join("o")),
            Box::new(Shared(Arc::new(Fake::default()))),
            "192.0.2.1:80",
        )
        .unwrap(),
    );
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    thread::spawn(move || far.serve(l));
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        crate::http::tests::send(addr, b"GET /v1/version HTTP/1.0\r\n\r\n"),
        ""
    );

    // And one whose edge is us answers.
    let near = Arc::clone(&t.e);
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    thread::spawn(move || near.serve(l));
    thread::sleep(Duration::from_millis(100));
    let out = crate::http::tests::send(addr, b"GET /v1/version HTTP/1.0\r\n\r\n");
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    assert!(out.contains(&format!("\"api\":\"{API_VERSION}\"")), "{out}");
}

#[test]
fn internal_errors_reach_the_audit_log_not_the_client() {
    let t = engine();
    let (_, owner) = t.primary();
    std::fs::write(t.d.0.join("state/engine.json"), "{ not json").unwrap();
    let (s, v) = t.call("GET", "/v1/status", Some(&owner), "");
    assert_eq!(s, 500);
    assert!(!v.to_string().contains("engine.json"), "{v}");
    assert!(t.audit_text().contains("engine.json"));
    // Unreadable state counts as locked down: a verb is refused, not run.
    assert_eq!(t.call("POST", "/v1/freeze", Some(&owner), "").0, 409);
    assert!(t.fake.calls().is_empty());
}
