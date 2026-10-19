use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new(label: &str) -> Self {
        let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "secondary-constructor-receiver-{label}-{}-{id}",
                std::process::id()
            ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, name: &str, source: &str) {
        fs::write(self.0.join(name), source).unwrap();
    }

    fn translate(&self, lombok: bool) {
        let out = self.0.join("out");
        let mut inputs = fs::read_dir(&self.0)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
            .collect::<Vec<_>>();
        inputs.sort();
        let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
        command
            .arg("--root")
            .arg(&self.0)
            .arg("--out-dir")
            .arg(&out)
            .args(["--annotations", "none"]);
        if lombok {
            command.arg("--lombok");
        }
        command.args(inputs);
        let output = command.output().expect("run notlin");
        assert!(
            output.status.success(),
            "notlin failed for Lombok={lombok}:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn out(&self, name: &str) -> PathBuf {
        self.0.join("out").join(name)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_common(fixture: &Fixture, token_method: &str, position_type: &str, constructor: &str) {
    fixture.write(
        "Token.kt",
        &format!(
            "package sample\nclass Token {{\n    {token_method}\n    suspend fun keep(): Unit = Unit\n}}\n"
        ),
    );
    fixture.write("Position.kt", &format!("package sample\n{position_type}\n"));
    fixture.write(
        "Snapshot.kt",
        &format!(
            "package sample\nimport java.util.UUID\nclass Snapshot(val first: UUID, val second: UUID) {{\n    {constructor}\n}}\n"
        ),
    );
}

fn assert_valid_fixture_sources(fixture: &Fixture) {
    let mut sources = fs::read_dir(&fixture.0)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect::<Vec<_>>();
    sources.sort();
    for path in sources {
        let name = path.file_name().unwrap().to_string_lossy();
        let source = fs::read_to_string(&path).unwrap();
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .unwrap();
        assert!(
            !parser.parse(&source, None).unwrap().root_node().has_error(),
            "invalid Kotlin fixture {name}:\n{source}"
        );
    }
}

#[test]
fn secondary_constructor_chained_property_call_translates_in_both_lombok_modes() {
    for lombok in [false, true] {
        let fixture = Fixture::new(if lombok {
            "positive-lombok"
        } else {
            "positive"
        });
        write_common(
            &fixture,
            "public fun read(): java.util.UUID = java.util.UUID.randomUUID()",
            "interface Position {\n val token: Token\n}",
            "constructor(position: Position) : this(position.token.read(), position.token.read())",
        );
        assert_valid_fixture_sources(&fixture);
        fixture.translate(lombok);

        assert!(fixture.out("Snapshot.java").is_file(), "Lombok={lombok}");
        assert!(fixture.0.join("Token.kt").is_file(), "Token stays Kotlin");
        assert!(
            !fixture.out("Token.java").exists(),
            "suspend Token must remain Kotlin"
        );
        let java = fs::read_to_string(fixture.out("Snapshot.java")).unwrap();
        assert!(
            java.contains("position.getToken().read()"),
            "secondary constructor should lower the chained call through the Java getter:\n{java}"
        );
        assert!(!java.contains("position.token"), "{java}");
    }
}

#[test]
fn non_plain_position_property_getters_keep_snapshot_kotlin() {
    let cases = [
        (
            "jvm-name-property-getter",
            "class Position(token: Token) {\n    @get:kotlin.jvm.JvmName(\"getRenamedToken\")\n    val token: Token = token\n    suspend fun keep(): Unit = Unit\n}",
        ),
        (
            "aliased-jvm-name-property-getter",
            "import kotlin.jvm.JvmName as GetterName\nclass Position(token: Token) {\n    @get:GetterName(\"getRenamedToken\")\n    val token: Token = token\n    suspend fun keep(): Unit = Unit\n}",
        ),
        (
            "jvm-field-property",
            "class Position(@JvmField val token: Token) {\n    suspend fun keep(): Unit = Unit\n}",
        ),
        (
            "synthetic-property-getter",
            "class Position(token: Token) {\n    @get:kotlin.jvm.JvmSynthetic\n    val token: Token = token\n    suspend fun keep(): Unit = Unit\n}",
        ),
        (
            "private-property",
            "class Position(private val token: Token) {\n    suspend fun keep(): Unit = Unit\n}",
        ),
        (
            "internal-property",
            "class Position(internal val token: Token) {\n    suspend fun keep(): Unit = Unit\n}",
        ),
        (
            "is-prefixed-property",
            "class Position(val isToken: Token) {\n    suspend fun keep(): Unit = Unit\n}",
        ),
    ];

    for (label, position) in cases {
        let fixture = Fixture::new(label);
        fixture.write(
            "Token.kt",
            "package sample\nclass Token {\n    public fun read(): java.util.UUID = java.util.UUID.randomUUID()\n    suspend fun keep(): Unit = Unit\n}\n",
        );
        fixture.write("Position.kt", &format!("package sample\n{position}\n"));
        fixture.write(
            "Snapshot.kt",
            "package sample\nimport java.util.UUID\nclass Snapshot(val first: UUID, val second: UUID) {\n    constructor(position: Position) : this(position.token.read(), position.token.read())\n}\n",
        );
        assert_valid_fixture_sources(&fixture);
        fixture.translate(false);
        assert!(
            fixture.0.join("Position.kt").is_file(),
            "{label}: suspend Position must remain Kotlin"
        );
        assert!(
            fixture.0.join("Snapshot.kt").is_file(),
            "{label}: Snapshot must remain Kotlin"
        );
        assert!(
            !fixture.out("Snapshot.java").exists(),
            "{label}: no partial Snapshot.java may be emitted"
        );
    }
}

#[test]
fn unsafe_secondary_constructor_chained_calls_keep_snapshot_kotlin() {
    let cases = [
        (
            "value-class-return",
            "@JvmInline value class ReadResult(val value: Int)\nclass Token {\n    public fun read(): ReadResult = ReadResult(1)\n    suspend fun keep(): Unit = Unit\n}",
            "interface Position {\n val token: Token\n}",
            "constructor(position: Position) : this(position.token.read(), position.token.read())",
            "ReadResult",
        ),
        (
            "private-method",
            "class Token {\n    private fun read(): java.util.UUID = java.util.UUID.randomUUID()\n    suspend fun keep(): Unit = Unit\n}",
            "interface Position {\n val token: Token\n}",
            "constructor(position: Position) : this(position.token.read(), position.token.read())",
            "java.util.UUID",
        ),
        (
            "internal-method",
            "class Token {\n    internal fun read(): java.util.UUID = java.util.UUID.randomUUID()\n    suspend fun keep(): Unit = Unit\n}",
            "interface Position {\n val token: Token\n}",
            "constructor(position: Position) : this(position.token.read(), position.token.read())",
            "java.util.UUID",
        ),
        (
            "jvm-name-method",
            "class Token {\n    @kotlin.jvm.JvmName(\"renamedRead\")\n    public fun read(): java.util.UUID = java.util.UUID.randomUUID()\n    suspend fun keep(): Unit = Unit\n}",
            "interface Position {\n val token: Token\n}",
            "constructor(position: Position) : this(position.token.read(), position.token.read())",
            "java.util.UUID",
        ),
        (
            "nullable-property",
            "class Token {\n    public fun read(): java.util.UUID = java.util.UUID.randomUUID()\n    suspend fun keep(): Unit = Unit\n}",
            "interface Position {\n val token: Token?\n}",
            "constructor(position: Position) : this(position.token!!.read(), position.token!!.read())",
            "java.util.UUID",
        ),
        (
            "vararg-position",
            "class Token {\n    public fun read(): java.util.UUID = java.util.UUID.randomUUID()\n    suspend fun keep(): Unit = Unit\n}",
            "interface Position {\n val token: Token\n}",
            "constructor(vararg position: Position) : this(position.token.read(), position.token.read())",
            "java.util.UUID",
        ),
    ];

    for (label, token, position, constructor, primary_type) in cases {
        let fixture = Fixture::new(label);
        fixture.write("Token.kt", &format!("package sample\n{token}\n"));
        fixture.write("Position.kt", &format!("package sample\n{position}\n"));
        fixture.write(
            "Snapshot.kt",
            &format!(
                "package sample\nclass Snapshot(val first: {primary_type}, val second: {primary_type}) {{\n    {constructor}\n}}\n"
            ),
        );
        if label != "private-method" && label != "nullable-property" && label != "vararg-position" {
            assert_valid_fixture_sources(&fixture);
        }
        fixture.translate(false);
        assert!(
            fixture.0.join("Snapshot.kt").is_file(),
            "{label}: unsafe receiver call must retain Snapshot"
        );
        assert!(
            !fixture.out("Snapshot.java").exists(),
            "{label}: no partial Snapshot.java may be emitted"
        );
    }
}

#[test]
fn own_default_companion_delegation_translates_in_both_lombok_modes() {
    for lombok in [false, true] {
        let fixture = Fixture::new(if lombok {
            "companion-positive-lombok"
        } else {
            "companion-positive"
        });
        fixture.write(
            "Token.kt",
            "package sample\nclass Token {\n    public fun read(): java.util.UUID = java.util.UUID.randomUUID()\n    suspend fun keep(): Unit = Unit\n}\n",
        );
        fixture.write(
            "Position.kt",
            "package sample\ninterface Position {\n val token: Token\n}\n",
        );
        fixture.write(
            "Snapshot.kt",
            "package sample\nimport java.util.UUID\nclass Snapshot(val first: UUID, val second: UUID) {\n    constructor(position: Position) : this(copy(position.token.read()), copy(position.token.read()))\n    companion object {\n        fun copy(value: UUID): UUID = value\n    }\n}\n",
        );
        assert_valid_fixture_sources(&fixture);
        fixture.translate(lombok);

        assert!(fixture.out("Snapshot.java").is_file(), "Lombok={lombok}");
        assert!(
            !fixture.out("Token.java").exists(),
            "suspend Token must remain Kotlin"
        );
        let java = fs::read_to_string(fixture.out("Snapshot.java")).unwrap();
        assert!(
            java.contains("static UUID copy(UUID value)"),
            "the own companion method should have a Java static declaration:\n{java}"
        );
        assert!(
            java.contains("copy(position.getToken().read())"),
            "the secondary delegation should call the translated own static method and Java getter:\n{java}"
        );
    }
}

#[test]
fn own_companion_property_arguments_require_accessor_evidence() {
    let fixture = Fixture::new("companion-renamed-property-argument");
    fixture.write(
        "Token.kt",
        "package sample\nclass Token {\n suspend fun keep() {}\n}\n",
    );
    fixture.write("Position.kt", "package sample\nclass Position(@get:kotlin.jvm.JvmName(\"renamedToken\") val token: java.util.UUID) {\n suspend fun keep() {}\n}\n");
    fixture.write("Snapshot.kt", "package sample\nimport java.util.UUID\nclass Snapshot(val first: UUID, val second: UUID) {\n constructor(position: Position) : this(copy(position.token), copy(position.token))\n companion object {\n  fun copy(value: UUID): UUID = value\n }\n}\n");
    assert_valid_fixture_sources(&fixture);
    fixture.translate(false);
    assert!(!fixture.out("Snapshot.java").exists());
}

#[test]
fn unsafe_own_companion_secondary_delegations_retain_snapshot() {
    let cases = [
        (
            "default-parameter",
            "companion object { fun copy(value: UUID = UUID(0, 0)): UUID = value }",
            "copy(position.token.read())",
            None,
        ),
        (
            "vararg-parameter",
            "companion object { fun copy(vararg values: UUID): UUID = values[0] }",
            "copy(position.token.read())",
            None,
        ),
        (
            "named-companion",
            "companion object Helpers { fun copy(value: UUID): UUID = value }",
            "copy(position.token.read())",
            None,
        ),
        (
            "jvm-name",
            "companion object { @kotlin.jvm.JvmName(\"renamedCopy\") fun copy(value: UUID): UUID = value }",
            "copy(position.token.read())",
            None,
        ),
        (
            "private-helper",
            "companion object { private fun copy(value: UUID): UUID = value }",
            "copy(position.token.read())",
            None,
        ),
        (
            "internal-helper",
            "companion object { internal fun copy(value: UUID): UUID = value }",
            "copy(position.token.read())",
            None,
        ),
        (
            "generic-helper",
            "companion object { fun <T> copy(value: T): T = value }",
            "copy(position.token.read())",
            None,
        ),
        (
            "ambiguous-overloads",
            "companion object { fun copy(value: UUID): UUID = value; fun copy(value: String): UUID = UUID(0, 0) }",
            "copy(position.token.read())",
            None,
        ),
        (
            "same-class-instance-overload",
            "fun copy(value: String): UUID = UUID(0, 0)\n    companion object { fun copy(value: UUID): UUID = value }",
            "copy(position.token.read())",
            None,
        ),
        (
            "named-argument",
            "companion object { fun copy(value: UUID): UUID = value }",
            "copy(value = position.token.read())",
            None,
        ),
        (
            "spread-argument",
            "companion object { fun copy(vararg values: UUID): UUID = values[0] }",
            "copy(*arrayOf(position.token.read()))",
            None,
        ),
        (
            "external-helper",
            "",
            "copy(position.token.read())",
            Some("package sample\nimport java.util.UUID\nfun copy(value: UUID): UUID = value\n"),
        ),
    ];

    for (label, helper, call, external) in cases {
        let fixture = Fixture::new(label);
        let helper = helper.replace("{ ", "{\n").replace(" }", "\n}");
        fixture.write(
            "Token.kt",
            "package sample\nclass Token {\n    public fun read(): java.util.UUID = java.util.UUID.randomUUID()\n    suspend fun keep(): Unit = Unit\n}\n",
        );
        fixture.write(
            "Position.kt",
            "package sample\ninterface Position {\n val token: Token\n}\n",
        );
        fixture.write(
            "Snapshot.kt",
            &format!(
                "package sample\nimport java.util.UUID\nclass Snapshot(val first: UUID, val second: UUID) {{\n    constructor(position: Position) : this({call}, {call})\n    {helper}\n}}\n"
            ),
        );
        if let Some(external) = external {
            fixture.write("External.kt", external);
        }
        assert_valid_fixture_sources(&fixture);
        fixture.translate(false);
        assert!(
            fixture.0.join("Snapshot.kt").is_file(),
            "{label}: Snapshot must remain Kotlin"
        );
        assert!(
            !fixture.out("Snapshot.java").exists(),
            "{label}: no partial Snapshot.java may be emitted"
        );
    }
}
