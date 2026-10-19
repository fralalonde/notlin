use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn json_annotations_move_back_from_property_repair_bridges_in_either_accessor_mode() {
    let root = Path::new("tests/tmp_property_repair_bridge_annotations");
    let _ = fs::remove_dir_all(root);
    for lombok in [false, true] {
        let case = root.join(if lombok { "lombok" } else { "explicit" });
        fs::create_dir_all(&case).unwrap();
        fs::write(
            case.join("SelfReferenceContext.java"),
            "package sample;\npublic interface SelfReferenceContext<T> {\n T getSelfReference();\n void setSelfReference(T value);\n}\n",
        )
        .unwrap();
        fs::write(
            case.join("GetterContext.java"),
            "package sample;\npublic interface GetterContext<T> { T getSelfReference(); }\n",
        )
        .unwrap();
        fs::write(
            case.join("SelfReference.kt"),
            r#"package sample

import com.fasterxml.jackson.annotation.JsonIgnore

class SelfReference : SelfReferenceContext<SelfReference> {
    @get:JsonIgnore
    @get:kotlin.jvm.JvmName("notlinPropertygetSelfReference")
    @set:kotlin.jvm.JvmName("notlinPropertysetSelfReference")
    var selfReference: SelfReference
        get() = this
        set(value) { backing = value }

    @JsonIgnore
    override fun getSelfReference(): SelfReference = selfReference

    @JsonIgnore
    override fun setSelfReference(value: SelfReference) { selfReference = value }

    private var backing: SelfReference? = null
}

class ConstructorSelfReference(
    @JvmField val selfReference: ConstructorSelfReference
) : GetterContext<ConstructorSelfReference> {
    @JsonIgnore
    override fun getSelfReference(): ConstructorSelfReference = selfReference
}
"#,
        )
        .unwrap();

        let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
        command
            .args(["--root", case.to_str().unwrap(), "--in-place"])
            .arg(case.join("SelfReference.kt"));
        if lombok {
            command.arg("--lombok");
        }
        let output = command.output().expect("run notlin");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "notlin failed:\n{stderr}");

        let java = fs::read_to_string(case.join("SelfReference.java")).unwrap();
        for signature in ["getSelfReference()", "setSelfReference(SelfReference"] {
            let signature_at = java
                .find(signature)
                .unwrap_or_else(|| panic!("expected {signature}:\n{java}"));
            let annotation_at = java[..signature_at]
                .rfind("@JsonIgnore")
                .unwrap_or_else(|| panic!("bridge annotation missing for {signature}:\n{java}"));
            assert!(
                signature_at - annotation_at < 80,
                "annotation must sit on {signature}:\n{java}"
            );
        }
        assert!(
            !java.contains("notlinProperty"),
            "temporary ABI leaked:\n{java}"
        );
        assert_eq!(
            java.matches("@JsonIgnore").count(),
            2,
            "the property's getter annotation and bridge annotation must merge onto one getter, alongside one setter annotation:\n{java}"
        );
        let constructor_java = fs::read_to_string(case.join("ConstructorSelfReference.java"))
            .unwrap_or_else(|_| panic!("constructor class was not emitted:\n{java}"));
        let constructor_class = constructor_java
            .find("class ConstructorSelfReference")
            .unwrap_or_else(|| panic!("constructor class was not emitted:\n{constructor_java}"));
        if lombok {
            let field = constructor_java[constructor_class..]
                .find("ConstructorSelfReference selfReference")
                .map(|offset| constructor_class + offset)
                .unwrap_or_else(|| {
                    panic!("expected constructor property field:\n{constructor_java}")
                });
            let annotation = constructor_java[..field]
                .rfind("@JsonIgnore")
                .unwrap_or_else(|| {
                    panic!("primary bridge annotation was lost:\n{constructor_java}")
                });
            assert!(
                field - annotation < 80,
                "Lombok fallback must preserve the annotation on the property field:\n{constructor_java}"
            );
        } else {
            let getter = constructor_java[constructor_class..]
                .find("getSelfReference()")
                .map(|offset| constructor_class + offset)
                .unwrap_or_else(|| {
                    panic!("expected constructor-property getter:\n{constructor_java}")
                });
            let annotation = constructor_java[..getter]
                .rfind("@JsonIgnore")
                .unwrap_or_else(|| {
                    panic!("primary bridge annotation was lost:\n{constructor_java}")
                });
            assert!(
                getter - annotation < 80,
                "annotation must sit on the constructor property's generated getter:\n{constructor_java}"
            );
        }
    }

    let _ = fs::remove_dir_all(root);
}
