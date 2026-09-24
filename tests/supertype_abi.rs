use std::fs;
use std::path::Path;
use std::process::Command;

/// A class implementing a RETAINED Kotlin interface whose abstract property
/// `items: List<Item>` conflicts with the class's `List<ItemImpl>` — Java
/// return types must match exactly. The class must stay Kotlin (taint) rather
/// than emit a Java twin javac rejects.
#[test]
fn retained_supertype_abi_mismatch_taints() {
    let root = Path::new("tests/tmp_scratch_abi");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("iface.kt"),
        "package a\n\n@Deprecated(\"keep\")\ninterface Item { val n: String }\n\n@Deprecated(\"keep\")\ninterface Box {\n    val items: List<Item>\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("impl.kt"),
        "package a\n\nclass ItemImpl(override val n: String) : Item\n\nclass Holder(\n    override val items: List<ItemImpl>\n) : Box\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("impl.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let out_java = fs::read_to_string(root.join("Holder.java")).unwrap_or_default();
    assert!(
        out_java.is_empty(),
        "ABI-mismatched class must stay Kotlin, got:\n{out_java}"
    );
    let _ = fs::remove_dir_all(root);
}

/// Transitive mismatch: Holder implements Box, Box extends DeepBox, and the
/// RETAINED DeepBox declares `items: List<Item>` while Holder has
/// `List<ItemImpl>` — the conflict surfaces two hops away and must taint.
#[test]
fn transitive_retained_supertype_abi_mismatch_taints() {
    let root = Path::new("tests/tmp_scratch_abi2");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("iface.kt"),
        "package b\n\n@Deprecated(\"keep\")\ninterface Item { val n: String }\n\n@Deprecated(\"keep\")\ninterface DeepBox {\n    val items: List<Item>\n}\n\ninterface Box : DeepBox\n",
    )
    .unwrap();
    fs::write(
        root.join("impl.kt"),
        "package b\n\nclass ItemImpl(override val n: String) : Item\n\nclass Holder(\n    override val items: List<ItemImpl>\n) : Box\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("impl.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let out_java = fs::read_to_string(root.join("Holder.java")).unwrap_or_default();
    assert!(
        out_java.is_empty(),
        "transitive ABI mismatch must keep the class Kotlin, got:\n{out_java}"
    );
    let _ = fs::remove_dir_all(root);
}

/// A Java implementation must not split a retained Kotlin interface diamond
/// whose property has an implementation on one branch. Kotlin's IR builds a
/// fake override for the diamond; leaving the implementation in Kotlin keeps
/// that override connected to its real declaration.
#[test]
fn retained_interface_property_diamond_keeps_implementation_kotlin() {
    let root = Path::new("tests/tmp_scratch_iface_diamond");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("contracts.kt"),
        "package neutral.diamond\n\nenum class Category { PRIMARY }\n\ninterface Root {\n    val category: Category\n}\n\ninterface Defaulted : Root {\n    override val category: Category\n        get() = Category.PRIMARY\n}\n\n@Deprecated(\"keep\")\ninterface Combined : Root, Defaulted\n",
    )
    .unwrap();
    fs::write(
        root.join("implementation.kt"),
        "package neutral.diamond\n\ndata class Concrete(val value: String) : Combined\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("implementation.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    assert!(
        output.status.success(),
        "notlin failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let out_java = fs::read_to_string(root.join("Concrete.java")).unwrap_or_default();
    assert!(
        out_java.is_empty(),
        "implementation of retained property diamond must stay Kotlin, got:\n{out_java}"
    );
    let kept = fs::read_to_string(root.join("implementation.kt")).unwrap();
    assert!(kept.contains("data class Concrete"));
    let _ = fs::remove_dir_all(root);
}

/// Exact property types are not enough to make a Java implementation safe
/// while its property-owning Kotlin interface remains in the mixed source set.
#[test]
fn retained_property_interface_keeps_exact_implementation_kotlin() {
    let root = Path::new("tests/tmp_scratch_iface_property");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("contract.kt"),
        "package neutral.property\n\n@Deprecated(\"keep\")\ninterface Contract {\n    val label: String\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("implementation.kt"),
        "package neutral.property\n\ndata class Exact(override val label: String) : Contract\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("implementation.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    assert!(
        output.status.success(),
        "notlin failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let out_java = fs::read_to_string(root.join("Exact.java")).unwrap_or_default();
    assert!(
        out_java.is_empty(),
        "exact implementation of retained property interface must stay Kotlin, got:\n{out_java}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn single_unambiguous_inherited_property_allows_translation() {
    let root = Path::new("tests/tmp_scratch_iface_single_inherited");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("contract.kt"),
        "package neutral.single\n\n@Deprecated(\"keep\")\ninterface Contract {\n    val label: String\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("implementation.kt"),
        "package neutral.single\n\nclass Inherited : Contract\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("implementation.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    assert!(
        output.status.success(),
        "notlin failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let out_java = fs::read_to_string(root.join("Inherited.java")).unwrap_or_default();
    assert!(
        !out_java.is_empty(),
        "single unambiguous inherited property must not retain the class"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn disjoint_properties_across_two_branches_allow_translation() {
    let root = Path::new("tests/tmp_scratch_iface_disjoint_branches");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("contracts.kt"),
        "package neutral.disjoint\n\n@Deprecated(\"keep\")\ninterface Left {\n    val left: String\n}\n\n@Deprecated(\"keep\")\ninterface Right {\n    val right: String\n}\n\ninterface Combined : Left, Right\n",
    )
    .unwrap();
    fs::write(
        root.join("implementation.kt"),
        "package neutral.disjoint\n\nclass Independent : Combined\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("implementation.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    assert!(
        output.status.success(),
        "notlin failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let out_java = fs::read_to_string(root.join("Independent.java")).unwrap_or_default();
    assert!(
        !out_java.is_empty(),
        "disjoint inherited properties must not retain the class"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn static_and_private_interface_properties_allow_translation() {
    let root = Path::new("tests/tmp_scratch_iface_non_inherited");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("contracts.kt"),
        "package neutral.noninherited\n\n@Deprecated(\"keep\")\ninterface StaticSide {\n    companion object {\n        val token: String get() = \"x\"\n    }\n}\n\n@Deprecated(\"keep\")\ninterface PrivateSide {\n    private val token: String get() = \"y\"\n}\n\ninterface Combined : StaticSide, PrivateSide\n",
    )
    .unwrap();
    fs::write(
        root.join("implementation.kt"),
        "package neutral.noninherited\n\nclass Safe(val token: String) : Combined\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("implementation.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    assert!(
        output.status.success(),
        "notlin failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let out_java = fs::read_to_string(root.join("Safe.java")).unwrap_or_default();
    assert!(
        !out_java.is_empty(),
        "static/private interface properties must not retain the class"
    );
    let _ = fs::remove_dir_all(root);
}
