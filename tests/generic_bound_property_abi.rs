use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_SCRATCH: AtomicUsize = AtomicUsize::new(0);

fn scratch(name: &str) -> PathBuf {
    let id = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("notlin-{name}-{}-{id}", std::process::id()))
}

fn run_notlin(root: &Path) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root)
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "notlin failed:\n{stderr}");
    stderr
}

#[test]
fn translates_readonly_contract_with_a_unique_generic_upper_bound() {
    let root = scratch("generic-bound-property-abi");
    fs::create_dir(&root).unwrap();
    fs::write(
        root.join("contract.kt"),
        "package neutral.genericbound\n\
         import com.fasterxml.jackson.annotation.JsonIgnore\n\
         interface ResourceId\n\
         interface ResourceAware { val resourceId: ResourceId }\n\
         interface GenericResourceEvent<S : ResourceId> : ResourceAware {\n\
         @get:JsonIgnore\n\
         override val resourceId: S\n\
         suspend fun load(): String\n\
         }\n",
    )
    .unwrap();

    let stderr = run_notlin(&root);
    let java = fs::read_to_string(root.join("ResourceAware.java")).unwrap_or_default();
    let retained = fs::read_to_string(root.join("contract.kt")).unwrap_or_default();
    assert!(
        java.contains("ResourceId getResourceId()"),
        "the concrete upper bound should make the parent getter bridgeable:\n{stderr}\n{java}"
    );
    assert!(
        retained.contains("interface GenericResourceEvent")
            && retained.contains("override fun getResourceId(): S")
            && retained.contains("@JsonIgnore")
            && !retained.contains("@get:JsonIgnore"),
        "the retained generic child must keep its JVM getter contract:\n{stderr}\n{retained}"
    );
    assert!(root.join("ResourceId.java").exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn keeps_contract_when_generic_getter_has_no_explicit_upper_bound() {
    let root = scratch("generic-bound-property-unbounded");
    fs::create_dir(&root).unwrap();
    fs::write(
        root.join("contract.kt"),
        "package neutral.genericboundunbounded\n\
         interface ResourceId\n\
         interface ResourceAware { val resourceId: ResourceId }\n\
         interface GenericResourceEvent<S> : ResourceAware {\n\
         override val resourceId: S\n\
         suspend fun load(): String\n\
         }\n",
    )
    .unwrap();

    let stderr = run_notlin(&root);
    assert!(
        !root.join("ResourceAware.java").exists(),
        "an unbounded type parameter cannot satisfy the inherited getter:\n{stderr}"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn retains_unsupported_generic_contracts() {
    for (name, declaration) in [
        (
            "nullable",
            "interface ResourceAware { val resourceId: ResourceId? }\ninterface Kept<S : ResourceId?> : ResourceAware {\n override val resourceId: S\n suspend fun load(): String\n}\n",
        ),
        (
            "writable",
            "interface ResourceAware { val resourceId: ResourceId }\ninterface Kept<S : ResourceId> : ResourceAware {\n override var resourceId: S\n suspend fun load(): String\n}\n",
        ),
        (
            "where",
            "interface ResourceAware { val resourceId: ResourceId }\ninterface Kept<S> : ResourceAware where S : ResourceId {\n override val resourceId: S\n suspend fun load(): String\n}\n",
        ),
        (
            "primitive",
            "interface ResourceAware { val resourceId: Int }\ninterface Kept<S : Int> : ResourceAware {\n override val resourceId: S\n suspend fun load(): String\n}\n",
        ),
        (
            "parameter-chain",
            "class Other : ResourceId\ninterface ResourceAware { val resourceId: ResourceId }\ninterface Kept<Other : ResourceId, S : Other> : ResourceAware {\n override val resourceId: S\n suspend fun load(): String\n}\n",
        ),
        (
            "unresolved-bound",
            "interface ResourceAware { val resourceId: missing.ExternalId }\ninterface Kept<S : missing.ExternalId> : ResourceAware {\n override val resourceId: S\n suspend fun load(): String\n}\n",
        ),
    ] {
        let root = scratch(name);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("Contracts.kt"),
            format!("package neutral\ninterface ResourceId\n{declaration}"),
        )
        .unwrap();
        let stderr = run_notlin(&root);
        assert!(
            !root.join("ResourceAware.java").exists(),
            "unsupported {name} bound must remain Kotlin:\n{stderr}"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
