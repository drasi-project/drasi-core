"""Check book links/excerpts and optionally exercise the copied starter.

Run from any directory:
    python3 lib/docs/developer-guide/check_book.py --test --network

The test copy uses the same sibling layout as the book. The repository itself
is linked, never cloned or modified by this checker.
"""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unicodedata


BOOK = Path(__file__).resolve().parent
REPOSITORY = BOOK.parents[2]
RUST_BLOCK = re.compile(
    r"<!-- (excerpt|illustration): ([^\n]+) -->\s*```rust\n(.*?)\n```", re.S
)


def rust_blocks(text):
    blocks = list(RUST_BLOCK.finditer(text))
    if len(blocks) != len(re.findall(r"^```rust$", text, re.M)):
        raise SystemExit("Every Rust fence needs an excerpt or illustration marker.")
    return blocks


def source_excerpt(reference):
    filename, anchor = reference.split("#", 1)
    path = (BOOK / filename).resolve()
    if not path.is_relative_to(BOOK / "stockroom"):
        raise SystemExit(f"Excerpt must come from the compiled starter: {reference}")
    source = path.read_text()
    start = f"// ANCHOR: {anchor}\n"
    end = f"// ANCHOR_END: {anchor}"
    if source.count(start) != 1 or source.count(end) != 1:
        raise SystemExit(f"Missing or duplicate source anchor: {reference}")
    return textwrap.dedent(source.split(start, 1)[1].split(end, 1)[0]).strip()


def sync_excerpts():
    path = BOOK / "README.md"
    text = path.read_text()
    rust_blocks(text)
    def replace(match):
        if match[1] == "illustration":
            return match[0]
        return f"<!-- excerpt: {match[2]} -->\n```rust\n{source_excerpt(match[2])}\n```"
    path.write_text(RUST_BLOCK.sub(replace, text))


def anchors(path):
    text = path.read_text()
    found = set(re.findall(r'<a\s+id="([^"]+)"', text))
    occurrences = {}
    fenced = False
    for line in text.splitlines():
        if line.startswith("```"):
            fenced = not fenced
            continue
        if fenced or not re.match(r"^#{1,6} ", line):
            continue
        title = re.sub(r"^#+ ", "", line).strip().lower()
        title = re.sub(r"\[([^\]]+)\]\([^)]+\)", r"\1", title)
        slug = "".join(
            char for char in title
            if char in "-_ " or unicodedata.category(char)[0] not in "PS"
        ).replace(" ", "-")
        count = occurrences.get(slug, 0)
        occurrences[slug] = count + 1
        found.add(slug if count == 0 else f"{slug}-{count}")
    return found


def check_book():
    path = BOOK / "README.md"
    text = path.read_text()
    failures = []
    links = 0
    if sum(line.startswith("```") for line in text.splitlines()) % 2:
        failures.append("unbalanced fenced code blocks")
    for match in re.finditer(r"(?<!!)\[[^\]]+\]\(([^)]+)\)", text):
        link = match[1]
        if re.match(r"^[a-zA-Z]+:", link):
            continue
        destination, _, fragment = link.partition("#")
        target = path.parent / destination if destination else path
        links += 1
        if not target.exists():
            failures.append(f"missing local link: {link}")
        elif fragment and target.suffix == ".md" and fragment not in anchors(target):
            failures.append(f"missing local anchor: {link}")
    chapters = re.findall(r"^### (\d+)\. ", text, re.M)
    if chapters != [str(index) for index in range(1, 19)]:
        failures.append(f"unexpected chapter sequence: {chapters}")
    if text.count("**Summary.**") != 18:
        failures.append("each numbered chapter must have one summary")
    if re.search(r"cargo (?:run|test|doc|build) --locked -p ", text):
        failures.append("workspace commands must permit first-run lockfile creation")
    excerpts = [block for block in rust_blocks(text) if block[1] == "excerpt"]
    for block in excerpts:
        if source_excerpt(block[2]) != textwrap.dedent(block[3]).strip():
            failures.append(f"excerpt drift: {block[2]}; run with --sync-excerpts")
    if failures:
        raise SystemExit("\n".join(failures))
    print(f"Checked {links} local links, 18 chapters, and {len(excerpts)} source-linked excerpts.", flush=True)


def compile_contexts(project):
    """Compile named illustrations; exact excerpts compile in the real starter."""
    blocks = rust_blocks((BOOK / "README.md").read_text())
    illustrations = {block[2]: block[3] for block in blocks if block[1] == "illustration"}
    if len(illustrations) != sum(block[1] == "illustration" for block in blocks):
        raise SystemExit("Illustration IDs must be unique.")
    provider = "provider: Arc<dyn drasi_core::computation::ComputationIndexProvider>"
    query = "query: ContinuousQueryTransformer"
    batch = "mut batch: ComponentBatchBuilder"
    contexts = {
        "component-members": "inventory: Inventory, query: ContinuousQueryTransformer, alerts: Alerts",
        "input-edge": batch,
        "readiness": "core: &DrasiLib, results: QueryResults",
        "quantity-patch": "items",
        "synthetic-join": "",
        "query-snapshot": "results: QueryResults",
        "output-snapshot-follow": "items",
        "chained-query-input": "items",
        "live-catalog": query,
        "source-pipeline": "expression",
        "continuation-chunk": "items",
        "byte-pipe": "expression",
        "time-merge": "",
        "native-index-provider": "",
        "delivery-tracking": query,
        "recovery-requirement": "",
        "sqlite-bootstrap": f"sqlite_config: drasi_source_sqlite::native::SqliteConfig, {query}",
        "bootstrap-binding": f"{query}, bootstrap_provider: Arc<dyn ComputationBootstrapProvider>",
        "channel-definition": "items",
        "envelope-codec": "items",
        "source-admission": provider,
        "admission-contract": "admission-method",
        "admission-retry": "admission: Arc<SourceAdmission>, input: ChangeEnvelope",
        "journal-binding": f"journal: Arc<QosChannel>, {batch}",
        "checkpointed-view": "items",
        "external-handler": "items",
        "source-transaction": "items",
        "transaction-container": f"definition: TransactionTransformerDefinition, registry: Arc<TransactionalTransformerRegistry>, {provider}",
        "shared-pipe": "channel: Arc<QosChannel>, journal_resource_id: ResourceId, group_resource_id: ResourceId",
        "query-factory": "",
        "provider-resource": f"{provider}, {batch}, indexes: ResourceId, specification: ComponentSpecification, factory: Arc<dyn ComponentFactory>",
        "identity-provider": "items",
        "secret-reference": "",
        "secret-resolver": "items",
        "apply-configuration": "items",
        "restart-preview": "core: &DrasiLib",
        "relationship-policy": batch,
        "owned-worker": "items",
        "quantity-validator": "items",
        "application-channel": "",
    }
    missing = illustrations.keys() - contexts.keys()
    if missing:
        raise SystemExit(f"Missing illustration contexts: {sorted(missing)}")
    common = """#![allow(dead_code, unused_imports, unused_variables, unused_mut, unused_assignments)]
use crate::*;
use crate::persistence::INSTANCE;
use drasi_core::interface::FailureMode;
use drasi_source_sqlite::native::SqliteSource;
"""
    # These shared helper functions are also taken directly from their fences.
    common += illustrations["channel-definition"] + "\n" + illustrations["envelope-codec"] + "\n"
    modules = [common]
    for name, code in illustrations.items():
        if not re.fullmatch(r"[a-z][a-z0-9-]*", name):
            raise SystemExit(f"Invalid illustration ID: {name}")
        context = contexts[name]
        if context == "items":
            body = code
        elif context == "admission-method":
            body = "struct Admitted { admission: Arc<SourceAdmission> }\nimpl Admitted {\n" + code + "\n}"
        elif context == "expression":
            body = "fn excerpt() -> anyhow::Result<()> { let _value = " + code + ";\nOk(())\n}"
        else:
            body = f"async fn excerpt({context}) -> anyhow::Result<()> {{\n{code}\nOk(())\n}}"
        modules.append(f"mod {name.replace('-', '_')} {{\nuse super::*;\n{body}\n}}\n")
    (project / "src/book_fragments.rs").write_text("\n".join(modules))
    with (project / "src/main.rs").open("a") as source:
        source.write(
            '\n#[cfg(all(test, feature = "normalize", feature = "persistence", '
            'feature = "plugins", feature = "management", feature = "callbacks"))]\nmod book_fragments;\n'
        )
    print(f"Compiling {len(illustrations)} illustrations and the source for {len(blocks)-len(illustrations)} exact excerpts.", flush=True)


def test_starter(test, network):
    environment = dict(os.environ)
    environment.setdefault("CARGO_TARGET_DIR", str(REPOSITORY / "target"))
    environment["RUST_LOG"] = "error"
    with tempfile.TemporaryDirectory(prefix="drasi-book-") as directory:
        root = Path(directory)
        (root / "drasi-core").symlink_to(REPOSITORY, target_is_directory=True)
        project = root / "stockroom"
        shutil.copytree(BOOK / "stockroom", project, ignore=shutil.ignore_patterns("target"))
        subprocess.run(["cargo", "fmt", "--", "--check"], cwd=project, env=environment, check=True)
        if test:
            compile_contexts(project)
            for arguments in [
                ["test", "--locked", "--quiet"],
                ["test", "--locked", "--quiet", "--features", "normalize,persistence,plugins,management,callbacks"],
            ]:
                subprocess.run(["cargo", *arguments], cwd=project, env=environment, check=True)
        if network:
            subprocess.run([
                "cargo", "build", "--quiet",
                "-p", "drasi-computation-source-http",
                "-p", "drasi-computation-sink-sse",
                "-p", "drasi-computation-sink-http",
                "--features", "dynamic-plugin",
            ], cwd=REPOSITORY, env=environment, check=True)
            build = subprocess.run([
                "cargo", "build", "--locked", "--quiet", "--features", "plugins",
                "--message-format=json",
            ], cwd=project, env=environment, stdout=subprocess.PIPE, text=True)
            if build.returncode:
                print(build.stdout, file=sys.stderr)
                build.check_returncode()
            executable = None
            for line in build.stdout.splitlines():
                message = json.loads(line)
                if (message.get("reason") == "compiler-artifact"
                        and message["target"]["name"] == "stockroom"
                        and message.get("executable")):
                    executable = Path(message["executable"])
            if executable is None:
                raise SystemExit("Cargo did not report the Stockroom executable.")
            subprocess.run([
                sys.executable, str(BOOK / "network_smoke.py"),
                "--binary", str(executable),
                "--libraries", str(executable.parent),
            ], cwd=project, env=environment, check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--test", action="store_true", help="also compile and test the complete starter")
    parser.add_argument("--network", action="store_true", help="build native plugins and smoke-test the network lessons (Linux/macOS)")
    parser.add_argument("--sync-excerpts", action="store_true", help="refresh marked excerpts from source")
    options = parser.parse_args()
    if options.sync_excerpts:
        sync_excerpts()
    check_book()
    if options.test or options.network:
        test_starter(options.test, options.network)


if __name__ == "__main__":
    main()
