import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const repository = new URL("../../../", import.meta.url);
const read = (path) => readFile(new URL(path, repository), "utf8");

function occurrences(source, marker) {
  return source.split(marker).length - 1;
}

test("parked tree removal exposes linear retained and indeterminate carriers", async () => {
  const source = await read("core/fs/src/lib.rs");

  assert.match(
    source,
    /pub enum DirectoryTreeRemovalOutcome \{[\s\S]*?Removed,[\s\S]*?Retained \{[\s\S]*?retained: RetainedDirectoryTreeRemoval,[\s\S]*?Indeterminate\(DirectoryTreeRemovalObligation\)/,
  );
  assert.match(
    source,
    /pub enum DirectoryTreeRemovalResolution \{[\s\S]*?Removed,[\s\S]*?Indeterminate\(DirectoryTreeRemovalObligation\)/,
  );
  assert.match(
    source,
    /pub fn remove_tree\(self\) -> DirectoryTreeRemovalOutcome/,
  );
  assert.match(
    source,
    /pub fn retain_parked_directory_tree_removal\([\s\S]*?OwnedEffect::ParkedDirectoryTreeRemoval/,
  );
  assert.match(
    source,
    /pub fn retain_directory_tree_removal\([\s\S]*?OwnedEffect::DirectoryTreeRemoval/,
  );
  assert.match(
    source,
    /Err\(error\) => DirectoryTreeRemovalOutcome::Indeterminate\(/,
  );
  assert.match(
    source,
    /Self::ParkedDirectoryTreeRemoval\(_\)[\s\S]*?Self::DirectoryTreeRemoval\(_\)[\s\S]*?=> true/,
  );
});

test("root reset and parked deletion share bounded iterative traversal", async () => {
  const platform = await read("core/fs/src/platform.rs");

  assert.equal(occurrences(platform, "fn clear_directory_children("), 2);
  assert.equal(
    occurrences(platform, "pub(crate) fn remove_parked_directory_tree("),
    2,
  );
  assert.equal(
    occurrences(platform, "pub(crate) const MAX_TREE_CLEAR_DEPTH: usize = 128"),
    2,
  );
  assert.equal(
    occurrences(platform, "const MAX_TREE_CLEAR_ENTRIES: usize = 1_000_000"),
    2,
  );
  assert.ok(
    occurrences(platform, "clear_directory_children(&root.handle") >= 2,
  );
  assert.ok(occurrences(platform, "clear_directory_children(&parked.") >= 2);
  assert.ok(occurrences(platform, "let mut stack = vec![ClearFrame") >= 2);
  assert.doesNotMatch(platform, /fn clear_directory_children[\s\S]*?remove_dir_all/);
});

test("tree traversal treats links and reparse points as retained leaves", async () => {
  const platform = await read("core/fs/src/platform.rs");

  assert.match(platform, /OFlags::PATH \| OFlags::NOFOLLOW \| OFlags::CLOEXEC/);
  assert.match(platform, /Linux offers no unprivileged fd-targeted unlink/);
  assert.match(platform, /AtFlags::SYMLINK_NOFOLLOW/);
  assert.match(platform, /FILE_OPEN_REPARSE_POINT/);
  assert.match(
    platform,
    /if observed_kind == EntryKind::Directory[\s\S]*?open_directory[\s\S]*?else \{[\s\S]*?remove_tree_/,
  );
  assert.match(
    platform,
    /directory_binding_state\(parent, &name, identity\)\? != BindingState::Exact/,
  );
});

test("Linux replacement guarantees stop at the cooperating authority boundary", async () => {
  const [library, architecture, ownership] = await Promise.all([
    read("core/fs/src/lib.rs"),
    read("docs/ARCHITECTURE.md"),
    read("docs/adr/0004-performance-internal-namespace-ownership.md"),
  ]);

  assert.match(library, /every descendant bound inside the claimed parked root/);
  assert.match(library, /Entries concurrently introduced[\s\S]*?deletion scope/);
  assert.match(architecture, /unprivileged POSIX has no atomic unlink-by-retained-fd/);
  assert.match(architecture, /malicious same-UID check\/unlink race is outside/);
  assert.match(ownership, /malicious same-UID writer can race/);
});

test("hostile tree-removal tests cover ownership and replacement boundaries", async () => {
  const source = await read("core/fs/src/lib.rs");

  for (const testName of [
    "effect_owner_removes_a_nonempty_parked_directory_tree",
    "parked_tree_removal_preserves_the_recreated_canonical_binding",
    "tree_removal_obligation_finishes_a_partially_cleared_root",
    "parked_tree_destination_case_equivalent_collision_is_no_effect",
    "parked_tree_removal_unlinks_links_without_following_them",
    "parked_tree_removal_preserves_a_replacement_root_binding",
    "bounded_tree_removal_can_be_retained_and_retried",
  ]) {
    assert.match(source, new RegExp(`fn ${testName}\\(`));
  }
});
