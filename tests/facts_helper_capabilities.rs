//! Production jar factories must not send Kotlin queries to Track A helpers.
//! Each probe runs in a bounded subprocess so a silent helper is a failing
//! regression, never an indefinitely hung Cargo suite.
#![cfg(unix)]

use notlin::facts::client::FactsClient;
use notlin::facts::protocol::{Answer, Query};
use notlin::facts::runtime::{self, FactsRuntime};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SOURCE: &str = "package capability\nclass Probe {\n fun convert(value: String): String = value.uppercase()\n}\n";

const HELPER: &str = r#"#!/bin/sh
kotlin=0
for argument do
  if [ "$argument" = '--kotlin' ]; then kotlin=1; fi
done
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CAPABILITY_REQUESTS"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"Ping":null'*)
      printf '{"version":1,"id":%s,"answer":{"Pong":{"version":1}}}\n' "$id" ;;
    *'"ClassMembers"'*)
      owner=$(printf '%s' "$line" | sed -n 's/.*"owner":"\([^"]*\)".*/\1/p')
      printf '{"version":1,"id":%s,"answer":{"Members":{"owner":"%s","members":[]}}}\n' "$id" "$owner" ;;
    *'"ResolveCalls"'*)
      if [ "$kotlin" = 1 ]; then
        file=$(printf '%s' "$line" | sed -n 's/.*"file":"\([^"]*\)".*/\1/p')
        printf '{"version":1,"id":%s,"answer":{"CallSites":{"file":"%s","sites":[]}}}\n' "$id" "$file"
      fi ;;
  esac
done
"#;

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn bounded_probe(api: &str, scenario: &str) {
    let root = Scratch(std::env::temp_dir().join(format!(
        "notlin-capabilities-{}-{api}-{scenario}",
        std::process::id()
    )));
    fs::create_dir_all(&root.0).unwrap();
    let helper = root.0.join("java-helper");
    fs::write(&helper, HELPER).unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(root.0.join("Probe.kt"), SOURCE).unwrap();
    let requests = root.0.join("requests.jsonl");
    let output = root.0.join("probe.log");
    let log = fs::File::create(&output).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "capability_probe", "--ignored", "--nocapture"])
        .env("CAPABILITY_ROOT", &root.0)
        .env("CAPABILITY_API", api)
        .env("CAPABILITY_SCENARIO", scenario)
        .env("CAPABILITY_REQUESTS", &requests)
        .env("NOTLIN_JAVA", helper)
        .env_remove("NOTLIN_JVM_FACTS_JAR")
        .env_remove("NOTLIN_KOTLIN_SOURCES")
        .env_remove("NOTLIN_CLASSPATH")
        .env_remove("NOTLIN_JVM_FACTS_KOTLIN")
        .env_remove("CAPABILITY_REAL_JAR")
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log));
    if scenario == "full-disabled" {
        command.env("NOTLIN_JVM_FACTS_KOTLIN", "0");
    } else if scenario == "renamed-enabled" {
        command.env("NOTLIN_JVM_FACTS_KOTLIN", "1");
    }
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let received = fs::read_to_string(&requests).unwrap_or_default();
    assert!(
        status.is_some(),
        "{api}/{scenario}: unsupported Kotlin query blocked the helper client; received:\n{received}\n{}",
        fs::read_to_string(&output).unwrap()
    );
    assert!(
        status.unwrap().success(),
        "{api}/{scenario}: {}",
        fs::read_to_string(&output).unwrap()
    );
    let enabled = matches!(scenario, "full-sources" | "renamed-enabled");
    let lines: Vec<serde_json::Value> = received
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(!lines.is_empty(), "probe did not exercise the helper");
    let kotlin_requests = lines
        .iter()
        .filter(|line| line["query"].get("ResolveCalls").is_some())
        .count();
    if enabled {
        assert!(
            kotlin_requests > 0,
            "enabled Kotlin queries never reached helper"
        );
    } else {
        assert_eq!(
            kotlin_requests, 0,
            "unsupported queries reached helper: {received}"
        );
        assert_eq!(
            lines.len(),
            2,
            "only Ping and ClassMembers are expected: {received}"
        );
        assert_eq!(lines[0]["id"], 0);
        assert_eq!(
            lines[1]["id"], 1,
            "skipped queries must not consume wire IDs"
        );
    }
}

#[test]
fn slim_runtime_skips_kotlin_queries_without_disabling_classpath_facts() {
    bounded_probe("runtime", "slim-sources");
}

#[test]
fn jar_launch_capabilities_cover_client_and_runtime_query_paths() {
    for api in ["client", "runtime"] {
        for scenario in [
            "slim-no-sources",
            "full-no-sources",
            "full-sources",
            "full-disabled",
            "renamed-enabled",
        ] {
            bounded_probe(api, scenario);
        }
    }
}

#[test]
#[ignore = "subprocess entry point, executed by the bounded capability tests"]
fn capability_probe() {
    let root =
        PathBuf::from(std::env::var_os("CAPABILITY_ROOT").expect("bounded parent supplies root"));
    let api = std::env::var("CAPABILITY_API").unwrap();
    let scenario = std::env::var("CAPABILITY_SCENARIO").unwrap();
    let source = root.join("Probe.kt");
    let enabled = matches!(scenario.as_str(), "full-sources" | "renamed-enabled");
    if scenario != "full-no-sources" && scenario != "slim-no-sources" {
        runtime::configure_kotlin_sources([&root]);
    }
    let jar = std::env::var_os("CAPABILITY_REAL_JAR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            root.join(if scenario.starts_with("slim") {
                "notlin-jvm-facts.jar"
            } else if scenario == "renamed-enabled" {
                "renamed-helper.jar"
            } else {
                "notlin-jvm-facts-full.jar"
            })
        });
    let query = Query::ResolveCalls {
        file: source.clone(),
    };
    let members = Query::ClassMembers {
        owner: "java.lang.String".into(),
    };
    if api == "client" {
        let mut client = FactsClient::start(&jar).unwrap();
        assert!(client.ping());
        assert_eq!(client.query(query.clone()).is_some(), enabled);
        assert_eq!(client.query_uncached(query).is_some(), enabled);
        let Some(Answer::Members {
            members: Some(facts),
            ..
        }) = client.query(members)
        else {
            panic!("Track A must remain available after skipped Kotlin queries");
        };
        if std::env::var_os("CAPABILITY_REAL_JAR").is_some() {
            assert!(
                !facts.is_empty(),
                "native Track A returned no String members"
            );
            eprintln!("native {api}/{scenario}: {} String members", facts.len());
        }
    } else {
        let runtime = FactsRuntime::start(&jar).unwrap();
        assert!(matches!(
            runtime.query(Query::Ping),
            Some(Answer::Pong { .. })
        ));
        assert_eq!(runtime.query(query.clone()).is_some(), enabled);
        assert_eq!(runtime.query_uncached(query).is_some(), enabled);
        assert_eq!(runtime.call_sites(&source).is_some(), enabled);
        assert!(
            runtime
                .call_site_at(&source, 0, blake3::hash(SOURCE.as_bytes()).as_bytes())
                .is_none()
        );
        let facts = runtime
            .member_facts("java.lang.String")
            .expect("Track A must remain available after skipped Kotlin queries");
        if std::env::var_os("CAPABILITY_REAL_JAR").is_some() {
            assert!(
                !facts.is_empty(),
                "native Track A returned no String members"
            );
            eprintln!("native {api}/{scenario}: {} String members", facts.len());
        }
    }
}
