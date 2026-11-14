use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
};

use crate::{
    cli,
    lockfile::Lockfile,
    manifest::{self, Manifest},
    target::Target,
};

use super::{
    AVAILABLE_BUILD_FLAGS, SOURCE_DIRECTORY, TYPE_BIN_ENTRY, TYPE_LIB_ENTRY, apply_pkg_args,
    compiler_command, ensure_lockfile, valid_output_name, validate_lockfile_or_exit,
};

const CHECK_ALL_ENTRY: &str = ".zora-check-all.zn";

const MODULE_KEYWORDS: &[&str] = &[
    "if",
    "else",
    "while",
    "for",
    "break",
    "continue",
    "let",
    "const",
    "comptime",
    "switch",
    "return",
    "pub",
    "fn",
    "extern",
    "include",
    "link",
    "use",
    "struct",
    "enum",
    "alias",
    "interface",
    "implement",
    "builtin",
    "type",
    "typeof",
    "true",
    "false",
    "nullptr",
    "self",
    "Self",
    "i8",
    "i16",
    "i32",
    "i64",
    "isize",
    "u8",
    "u16",
    "u32",
    "u64",
    "usize",
    "f32",
    "f64",
    "bool",
    "char",
    "void",
    "never",
];

pub fn cmd_check(manifest: &Manifest, root: &Path, avoid_execution: bool, all: bool) -> Lockfile {
    // Lockfile and Zeen version checks

    let lockfile = ensure_lockfile(root, manifest);
    validate_lockfile_or_exit(&lockfile);

    // Paths checks

    let src_available = root.join("src").is_dir();

    if !src_available {
        cli::exit_with_error("Source directory `src` not found");
    }

    let entry_file = match manifest.package.r#type {
        manifest::PackageType::Bin => TYPE_BIN_ENTRY,
        manifest::PackageType::Static | manifest::PackageType::Dynamic => TYPE_LIB_ENTRY,
    };
    let entry_path = root.join(SOURCE_DIRECTORY).join(entry_file);

    if !entry_path.is_file() {
        cli::exit_with_error(format!(
            "Entry source not found (for your package its `{}/{}`)",
            SOURCE_DIRECTORY, entry_file
        ));
    }

    // Build section checks

    if let Some(ref build_section) = manifest.build {
        if let Some(ref output) = build_section.output
            && !valid_output_name(output)
        {
            cli::exit_with_error("Field `[build].output` should have only name without extension");
        }

        validate_flags(build_section.flags.as_deref());

        for over in build_section.overrides() {
            if let Some(ref output) = over.output
                && !valid_output_name(output)
            {
                cli::exit_with_error(
                    "Field `[build].output` should have only name without extension",
                );
            }

            validate_flags(over.flags.as_deref());
        }
    }

    // Zeen sources check (via `--check` flag)

    if avoid_execution {
        return lockfile;
    }

    let src_dir = root.join(SOURCE_DIRECTORY);
    let target = Target::host();
    let started = std::time::Instant::now();

    cli::println_info(
        "Starting",
        format!("checks for project (root: `{}`)", root.display()),
    );
    cli::println_info("Target", format!("triple `{}`", target.triple));

    let emit_object = manifest.package.r#type != manifest::PackageType::Bin;

    if !run_check(
        root,
        &src_dir,
        &lockfile,
        Path::new(entry_file),
        emit_object,
    ) {
        std::process::exit(1);
    }

    if all && !check_all_modules(root, &src_dir, entry_file, &lockfile) {
        std::process::exit(1);
    }

    cli::println_info(
        "Check",
        format!(
            "all checks passed (in {:.2}s)",
            started.elapsed().as_secs_f64()
        ),
    );

    lockfile
}

fn check_all_modules(root: &Path, src_dir: &Path, entry_file: &str, lockfile: &Lockfile) -> bool {
    let entry_path = src_dir.join(CHECK_ALL_ENTRY);

    let mut modules: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    let mut standalone: Vec<PathBuf> = Vec::new();

    for file in source_files(src_dir) {
        if file == entry_path {
            continue;
        }

        let relative = file.strip_prefix(src_dir).unwrap_or(&file);

        if relative == Path::new(entry_file) {
            continue;
        }

        match module_name(relative) {
            Some(module) => modules.entry(module).or_default().push(file.clone()),
            None => standalone.push(file.clone()),
        }
    }

    let mut imports: Vec<String> = Vec::new();

    for (module, files) in modules {
        if files.len() == 1 {
            imports.push(module);
        } else {
            standalone.extend(files);
        }
    }

    if !imports.is_empty() && !check_generated_entry(root, src_dir, &entry_path, &imports, lockfile)
    {
        return false;
    }

    standalone.sort();
    standalone.dedup();

    for file in standalone {
        let relative = file.strip_prefix(src_dir).unwrap_or(&file);

        if !run_check(root, src_dir, lockfile, relative, true) {
            return false;
        }
    }

    true
}

fn check_generated_entry(
    root: &Path,
    src_dir: &Path,
    entry: &Path,
    modules: &[String],
    lockfile: &Lockfile,
) -> bool {
    let mut contents = String::new();

    for module in modules {
        contents.push_str("use root.");
        contents.push_str(module);
        contents.push_str(";\n");
    }

    fs::write(entry, contents).unwrap_or_else(|err| {
        cli::exit_with_error(format!(
            "Unable to write `{}`: {err}",
            entry.to_string_lossy()
        ))
    });

    let success = run_check(root, src_dir, lockfile, entry, true);

    let _ = fs::remove_file(entry);

    success
}

fn run_check(
    root: &Path,
    src_dir: &Path,
    lockfile: &Lockfile,
    entry: &Path,
    emit_object: bool,
) -> bool {
    let mut command = compiler_command(src_dir);
    apply_pkg_args(&mut command, lockfile, root);

    command.arg(entry).arg("--check");

    if emit_object {
        command.arg("--emit").arg("OBJ");
    }

    let output = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap_or_else(|err| cli::exit_with_error(format!("Check command error: {}", err)));

    report_check_output(&output);

    output.status.success()
}

fn report_check_output(output: &std::process::Output) {
    for stream in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(stream);

        for line in text.lines() {
            if line.starts_with("Building from ")
                || line.starts_with("Target triple ")
                || line.starts_with("Check all checks passed")
            {
                continue;
            }

            eprintln!("{line}");
        }
    }
}

fn source_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();

    collect_source_files(dir, &mut files);
    files.sort();

    files
}

fn collect_source_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|err| {
        cli::exit_with_error(format!("Unable to read `{}`: {err}", dir.to_string_lossy()))
    });

    for entry in entries {
        let entry = entry.unwrap_or_else(|err| {
            cli::exit_with_error(format!("Unable to read `{}`: {err}", dir.to_string_lossy()))
        });

        let path = entry.path();

        if path.is_dir() {
            collect_source_files(&path, files);
            continue;
        }

        if path.extension().and_then(|extension| extension.to_str()) == Some("zn") {
            files.push(path);
        }
    }
}

fn module_name(relative: &Path) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();

    for component in relative.components() {
        let Component::Normal(part) = component else {
            return None;
        };

        parts.push(part.to_str()?);
    }

    let stem = parts.last()?.strip_suffix(".zn")?;
    let last = parts.last_mut()?;
    *last = stem;

    if *last == "index" {
        parts.pop();
    }

    if parts.is_empty() || parts.iter().any(|part| MODULE_KEYWORDS.contains(part)) {
        return None;
    }

    Some(parts.join("."))
}

fn validate_flags(flags: Option<&[String]>) {
    let Some(flags) = flags else {
        return;
    };

    flags.iter().for_each(|flag| {
        if !AVAILABLE_BUILD_FLAGS.contains(&flag.as_str()) {
            cli::exit_with_error(format!("Field `[build].flags` doesn't have '{}'", flag));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_name_joins_nested_paths() {
        assert_eq!(
            module_name(Path::new("collections/list.zn")),
            Some("collections.list".to_owned())
        );
    }

    #[test]
    fn module_name_uses_directory_for_index_file() {
        assert_eq!(
            module_name(Path::new("collections/index.zn")),
            Some("collections".to_owned())
        );
    }

    #[test]
    fn module_name_rejects_keyword_segments() {
        assert_eq!(module_name(Path::new("fn.zn")), None);
        assert_eq!(module_name(Path::new("primitives/i8.zn")), None);
    }

    #[test]
    fn module_name_rejects_root_index() {
        assert_eq!(module_name(Path::new("index.zn")), None);
    }

    #[test]
    fn module_name_rejects_non_source_files() {
        assert_eq!(module_name(Path::new("README.md")), None);
    }
}
