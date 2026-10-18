"""Check book links/excerpts and optionally exercise the copied starter.

Run from any directory:
    python3 lib/docs/developer-guide/check_book.py --test

The test copy uses the same sibling layout as the book. The repository itself
is linked, never cloned or modified by this checker.
"""

import argparse
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unicodedata


BOOK = Path(__file__).resolve().parent
REPOSITORY = BOOK.parents[2]


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
    excerpts = re.findall(
        r"<!-- excerpt: ([^#\s]+)#([^\s]+) -->\s*```rust\n(.*?)\n```",
        text,
        re.S,
    )
    for filename, anchor, excerpt in excerpts:
        source = (BOOK / filename).read_text()
        start = f"// ANCHOR: {anchor}\n"
        end = f"// ANCHOR_END: {anchor}"
        if source.count(start) != 1 or source.count(end) != 1:
            failures.append(f"missing or duplicate source excerpt {filename}#{anchor}")
            continue
        actual = source.split(start, 1)[1].split(end, 1)[0]
        if re.sub(r"\s+", " ", actual).strip() != re.sub(r"\s+", " ", excerpt).strip():
            failures.append(f"excerpt drift: {filename}#{anchor}")
    if failures:
        raise SystemExit("\n".join(failures))
    print(f"Checked {links} local links, 18 chapters, and {len(excerpts)} source-linked excerpts.", flush=True)


def compile_contexts(project):
    """Give contextual excerpts their surrounding types without executing them.

    Each entry corresponds to one Rust fence, in book order. The code inside
    the fence always comes from the book, never from a second maintained copy.
    Adding/removing a fence requires updating its explicit context here.
    """
    blocks = re.findall(r"^```rust\n(.*?)^```", (BOOK / "README.md").read_text(), re.M | re.S)
    provider = "provider: Arc<dyn drasi_core::computation::ComputationIndexProvider>"
    query = "query: ContinuousQueryTransformer"
    batch = "mut batch: ComponentBatchBuilder"
    contexts = [
        "",  # query construction
        "inventory: Inventory, query: ContinuousQueryTransformer, alerts: Alerts",
        batch,
        batch,
        "core: &DrasiLib, results: QueryResults",
        "product",
        "items",
        "items",
        "source-method",
        "",
        "sink-impl",
        "items",
        "view-impl",
        "results: QueryResults",
        query,
        "audit-method",
        "",
        "expression",
        "items",
        "expression",
        "",
        "items",
        "",
        f"definition: ContinuousQueryDefinition, {provider}",
        query,
        "",
        f"sqlite_config: drasi_source_sqlite::native::SqliteConfig, {query}",
        f"{query}, bootstrap_provider: Arc<dyn ComputationBootstrapProvider>",
        "items",
        "items",
        provider,
        "admission-method",
        "admission: Arc<SourceAdmission>, input: ChangeEnvelope",
        provider,
        f"outgoing: Arc<QosChannel>, {batch}",
        "items",
        provider,
        "items",
        "items",
        "items",
        f"definition: TransactionTransformerDefinition, registry: Arc<TransactionalTransformerRegistry>, {provider}",
        "channel: Arc<QosChannel>, journal_resource_id: ResourceId, group_resource_id: ResourceId",
        "",
        f"{provider}, {batch}, indexes: ResourceId, specification: ComponentSpecification, factory: Arc<dyn ComponentFactory>",
        "items",
        "",
        "items",
        batch,
        "factories: FactoryRegistry, resources: Arc<dyn drasi_lib::management::ManagementResourceResolver>, store: Arc<dyn drasi_lib::management::ConfigurationStore>",
        "items",
        "core: &DrasiLib",
        "",
        "items",
        "items",
        "path: &Path, mut factories: FactoryRegistry",
    ]
    if len(blocks) != len(contexts):
        raise SystemExit(f"{len(blocks)} Rust fences need explicit contexts; configured {len(contexts)}")
    common = """#![allow(dead_code, unused_imports, unused_variables, unused_mut, unused_assignments)]
use crate::*;
use drasi_core::interface::FailureMode;
use drasi_source_sqlite::native::SqliteSource;
"""
    # These shared helper functions are also taken directly from their fences.
    common += blocks[28] + "\n" + blocks[29] + "\n"
    modules = [common]
    for number, (code, context) in enumerate(zip(blocks, contexts), 1):
        if context == "items":
            body = code
        elif context == "product":
            body = "struct Product { sku: String, quantity: i64, minimum: i64, warehouse: String }\n" + code
        elif context == "view-impl":
            body = "struct ResultView(HashMap<RecordId, serde_json::Value>);\n" + code
        elif context == "source-method":
            body = (
                "impl Inventory { fn excerpt(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {\n"
                + code + "\nOk(None)\n} }"
            )
        elif context == "audit-method":
            body = (
                "impl Audit { async fn excerpt(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {\n"
                + code + "\n} }"
            )
        elif context == "admission-method":
            body = "struct Admitted { admission: Arc<SourceAdmission> }\nimpl Admitted {\n" + code + "\n}"
        elif context == "sink-impl":
            body = """struct Alerts {
    descriptor: ComponentDescriptor,
    query_id: &'static str,
    view: watch::Sender<ResultView>,
    output: tokio::io::Stdout,
}
#[async_trait]
impl ComputationComponent for Alerts {
    fn descriptor(&self) -> &ComponentDescriptor { &self.descriptor }
    async fn start(&mut self) -> anyhow::Result<()> { Ok(()) }
    async fn stop(&mut self) -> anyhow::Result<()> { Ok(()) }
}
""" + code
        elif context == "expression":
            body = "fn excerpt() -> anyhow::Result<()> { let _value = " + code + ";\nOk(())\n}"
        else:
            body = f"async fn excerpt({context}) -> anyhow::Result<()> {{\n{code}\nOk(())\n}}"
        modules.append(f"mod fence_{number:02} {{\nuse super::*;\n{body}\n}}\n")
    (project / "src/book_fragments.rs").write_text("\n".join(modules))
    with (project / "src/main.rs").open("a") as source:
        source.write(
            '\n#[cfg(all(test, feature = "normalize", feature = "persistence", '
            'feature = "plugins", feature = "management"))]\nmod book_fragments;\n'
        )
    print(f"Compiling all {len(blocks)} Rust fences in explicit lesson contexts.", flush=True)


def test_starter():
    environment = dict(os.environ)
    environment.setdefault("CARGO_TARGET_DIR", str(REPOSITORY / "target"))
    environment["RUST_LOG"] = "error"
    with tempfile.TemporaryDirectory(prefix="drasi-book-") as directory:
        root = Path(directory)
        (root / "drasi-core").symlink_to(REPOSITORY, target_is_directory=True)
        project = root / "stockroom"
        shutil.copytree(BOOK / "stockroom", project, ignore=shutil.ignore_patterns("target"))
        subprocess.run(["cargo", "fmt", "--", "--check"], cwd=project, env=environment, check=True)
        compile_contexts(project)
        for arguments in [
            ["test", "--locked", "--quiet"],
            ["test", "--locked", "--quiet", "--features", "normalize,persistence,plugins,management"],
        ]:
            subprocess.run(["cargo", *arguments], cwd=project, env=environment, check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--test", action="store_true", help="also compile and test the complete starter")
    options = parser.parse_args()
    check_book()
    if options.test:
        test_starter()


if __name__ == "__main__":
    main()
