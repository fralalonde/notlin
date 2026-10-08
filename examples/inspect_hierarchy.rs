//! Read-only inspection of a resolved declaration and its indexed descendants.

use notlin::semantics::workspace_symbol;
use notlin::workspace::SourceIndex;
use std::collections::BTreeMap;
use std::path::PathBuf;

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().ok_or("expected workspace root")?);
    let requested = args.next().ok_or("expected qualified declaration name")?;
    let index = SourceIndex::discover(&root)?;
    let simple = requested.rsplit('.').next().unwrap_or(&requested);
    let candidates = index
        .declarations_named(simple)
        .filter(|declaration| {
            !requested.contains('.')
                || declaration
                    .package
                    .as_ref()
                    .map(|package| format!("{package}.{}", declaration.name))
                    .as_deref()
                    == Some(requested.as_str())
        })
        .collect::<Vec<_>>();
    let [declaration] = candidates.as_slice() else {
        return Err(format!(
            "expected one root declaration, found {}",
            candidates.len()
        ));
    };
    let mut pending = vec![*declaration];
    let mut found = BTreeMap::new();
    while let Some(declaration) = pending.pop() {
        let symbol = workspace_symbol(&index, declaration);
        if found.contains_key(&symbol) {
            continue;
        }
        pending.extend(index.direct_subtypes(declaration));
        found.insert(symbol, declaration);
    }
    for (symbol, declaration) in &found {
        println!(
            "{:?}\t{:?}\t{}.{}\t{}",
            declaration.language,
            declaration.kind,
            symbol.package,
            symbol.name,
            symbol.file.display()
        );
    }
    eprintln!("{} declaration(s), including the root", found.len());
    Ok(())
}
