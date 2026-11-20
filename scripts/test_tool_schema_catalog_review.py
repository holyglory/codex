"""Static proof regressions; no Codex process or snapshot acceptance is involved."""

import copy
import unittest

from review_tool_schema_hashes import fingerprint
from tool_schema_catalog_review import review_body
from tool_schema_catalog_review import reconstruct_catalog


BASE = {
    "type": "function",
    "name": "run",
    "description": "Before.",
    "parameters": {"properties": {"first": {}}},
}
UPDATED = {
    "type": "function",
    "name": "run",
    "description": "After.",
    "parameters": {"properties": {"second": {}}},
}
APPROVED = [
    {"type": "function", "name": name}
    for name in ("account_management", "usage_stats", "usage_activity")
]
REMOTE = {
    "type": "namespace",
    "name": "remote",
    "tools": [{"type": "function", "name": "first"}],
}
REMOTE_UPDATED = {
    "type": "namespace",
    "name": "remote",
    "tools": [{"type": "function", "name": "second"}],
}


def block(catalog, rows, prefix="  tools "):
    return f"{prefix}({len(catalog)}; hash={fingerprint(catalog)}):\n{rows}"


def approved_rows(marker="-"):
    return "".join(
        f"    {marker} function/{tool['name']}; hash={fingerprint(tool)}\n"
        for tool in APPROVED
    )


def initial_rows(current=False):
    return (
        f"    - function/run: Before.; args=[first]; hash={fingerprint(BASE)}\n"
        + (approved_rows() if current else "")
        + f"    - namespace/remote; hash={fingerprint(REMOTE)}\n"
        + "      - function/first\n"
    )


class CatalogReviewTests(unittest.TestCase):
    def setUp(self):
        self.old = [BASE, REMOTE]
        self.new = [BASE, *APPROVED, REMOTE]
        self.old_next = [UPDATED, REMOTE_UPDATED]
        self.new_next = [UPDATED, *APPROVED, REMOTE_UPDATED]
        self.catalogs = {
            fingerprint(self.old): (self.old, self.new),
            fingerprint(self.old_next): (self.old_next, self.new_next),
            fingerprint([]): ([], []),
        }
        self.before = "Scenario: proof.\n\n## Window 1\nSettings:\n" + block(
            self.old, initial_rows()
        )
        self.after = "Scenario: proof.\n\n## Window 1\nSettings:\n" + block(
            self.new, initial_rows(True)
        )
        lineage = (
            "-- request 1 (turn) --\n\n## Window 2 (after request 1)\n"
            "Settings: same as window 1\n-- request 2 (turn) --\n\n"
            "## Window 3 (after request 2)\nSettings: relative to window 2\n"
        )
        delta = (
            f"    ~ function/run: After.; args=[second]; hash={fingerprint(UPDATED)}\n"
            f"    ~ namespace/remote; hash={fingerprint(REMOTE_UPDATED)}\n"
            "      - function/second\n"
        )
        self.before += lineage + block(self.old_next, delta)
        self.after += lineage + block(self.new_next, delta)

    def review(self, before=None, after=None, catalogs=None):
        return review_body(
            (self.before if before is None else before).splitlines(True),
            (self.after if after is None else after).splitlines(True),
            self.catalogs if catalogs is None else catalogs,
            fingerprint,
        )

    def test_exact_delta_after_same_as_and_namespace_changes(self):
        result, used = self.review()
        self.assertEqual(result, self.after)
        self.assertEqual(sum(used.values()), 2)

    def test_rejects_non_tool_or_tool_text_tampering(self):
        mutations = [
            ("Scenario: proof.", "Scenario: changed."),
            ("(5; hash=", "(6; hash="),
            ("After.", "Other."),
            ("args=[second]", "args=[first]"),
            ("      - function/second", "      - function/first"),
            ("    ~ function/run", "    + function/run"),
            (fingerprint(self.new_next), "0000000000000000"),
            (fingerprint(UPDATED), "0000000000000000"),
        ]
        for old, new in mutations:
            with self.subTest(mutation=(old, new)), self.assertRaises(ValueError):
                self.review(after=self.after.replace(old, new, 1))

    def test_empty_catalog_requires_exact_zero_count_and_hash(self):
        lineage = "\n## Window 4\nSettings: relative to window 3\n"
        old_rows = "    - removed function/run\n    - removed namespace/remote\n"
        new_rows = (
            "    - removed function/run\n"
            + "".join(f"    - removed function/{tool['name']}\n" for tool in APPROVED)
            + "    - removed namespace/remote\n"
        )
        before = self.before + lineage + block([], old_rows)
        after = self.after + lineage + block([], new_rows)
        self.assertEqual(self.review(before, after)[0], after)
        for altered in [
            after.replace("(0; hash=", "(1; hash="),
            after.replace(fingerprint([]), "0000000000000000"),
        ]:
            with self.subTest(altered=altered[-100:]), self.assertRaises(ValueError):
                self.review(before, altered)

    def test_missing_exact_predecessor_is_rejected(self):
        predecessor = block(self.old, initial_rows())
        before = self.before
        after = self.after.replace(block(self.new, initial_rows(True)), predecessor, 1)
        catalogs = dict(self.catalogs)
        del catalogs[fingerprint(self.old)]
        with self.assertRaisesRegex(ValueError, "predecessor.*preimage"):
            self.review(before, after, catalogs)

    def test_invalid_lineage_is_rejected_even_when_text_is_unchanged(self):
        for old, new in [
            ("same as window 1", "same as window 9"),
            ("relative to window 2", "relative to window 1"),
            ("relative to window 2", "relative to window 9"),
        ]:
            with self.subTest(lineage=new), self.assertRaises(ValueError):
                self.review(self.before.replace(old, new), self.after.replace(old, new))

    def test_unavailable_or_removed_tools_reset_the_predecessor(self):
        prefix_old = self.before.split("-- request 1")[0]
        prefix_new = self.after.split("-- request 1")[0]
        for middle, settings in [
            ("Settings: unavailable (items only)\n", "Settings:\n"),
            (
                "Settings: relative to window 1\n  tools: <removed>\n",
                "Settings: relative to window 2\n",
            ),
        ]:
            lineage = "\n## Window 2\n" + middle + "\n## Window 3\n" + settings
            before = prefix_old + lineage + block(self.old, initial_rows())
            after = prefix_new + lineage + block(self.new, initial_rows(True))
            with self.subTest(middle=middle):
                self.assertEqual(self.review(before, after)[0], after)

    def test_order_and_ambiguous_names_render_exact_sentinels(self):
        prefix_old = self.before.split("-- request 1")[0]
        prefix_new = self.after.split("-- request 1")[0]
        for old, current, sentinel in [
            ([REMOTE, BASE], [REMOTE, BASE, *APPROVED], "order changed"),
            (
                [BASE, BASE],
                [BASE, BASE, *APPROVED],
                "inventory changed (ambiguous tool names)",
            ),
        ]:
            catalogs = dict(self.catalogs)
            catalogs[fingerprint(old)] = (old, current)
            lineage = "\n## Window 2\nSettings: relative to window 1\n"
            row = f"    - {sentinel}\n"
            with self.subTest(sentinel=sentinel):
                before = prefix_old + lineage + block(old, row)
                after = prefix_new + lineage + block(current, row)
                self.assertEqual(self.review(before, after, catalogs)[0], after)

    def test_portable_equal_raw_change_is_rejected_without_raw_evidence(self):
        prefix_old = self.before.split("-- request 1")[0]
        prefix_new = self.after.split("-- request 1")[0]
        lineage = "\n## Window 2\nSettings: relative to window 1\n"
        row = f"    ~ function/run: Before.; args=[first]; hash={fingerprint(BASE)}\n"
        with self.assertRaises(ValueError):
            self.review(
                prefix_old + lineage + block(self.old, row),
                prefix_new + lineage + block(self.new, row),
            )

    def test_additional_tools_are_full_inventories_after_settings(self):
        before = self.before + "-- request 3 (turn) --\n"
        after = self.after + "-- request 3 (turn) --\n"
        old_rows = "".join("    " + line for line in initial_rows().splitlines(True))
        new_rows = "".join(
            "    " + line for line in initial_rows(True).splitlines(True)
        )
        prefix = "00:additional_tools/developer "
        before += block(self.old, old_rows, prefix)
        after += block(self.new, new_rows, prefix)
        self.assertEqual(self.review(before, after)[0], after)
        delta = (
            f"        ~ function/run: Before.; args=[first]; hash={fingerprint(BASE)}\n"
        )
        with self.assertRaises(ValueError):
            self.review(before, after.replace(new_rows, delta))


class ContractVariantTests(unittest.TestCase):
    def setUp(self):
        self.tools = [
            {
                "type": "function",
                "name": tool["name"],
                "description": "Approved.",
                "strict": False,
                "parameters": {
                    "type": "object",
                    "additionalProperties": False,
                    "properties": {"action": {"type": "string", "enum": ["list"]}},
                    "required": ["action"],
                },
            }
            for tool in APPROVED
        ]
        self.variants = copy.deepcopy(self.tools)
        sections = {}
        for variant in self.variants:
            name = variant["name"]
            variant["description"] = (
                "Approved.\n\nexec tool declaration:\n```ts\n"
                f'declare const tools: {{ {name}(args: {{ action: "list"; }}): Promise<unknown>; }};\n```'
            )
            sections[name] = f"### `{name}`\n" + variant["description"]
        self.contract = {
            "schemaVersion": 2,
            "tools": self.tools,
            "sections": sections,
            "codeModeTools": self.variants,
        }

    def test_both_exact_production_forms_are_accepted(self):
        self.assertEqual(reconstruct_catalog(self.tools, self.contract), [])
        self.assertEqual(reconstruct_catalog(self.variants, self.contract), [])

    def test_changed_captured_fields_are_rejected(self):
        for field, value in [
            ("strict", True),
            ("defer_loading", True),
            ("parameters", {}),
            ("description", "Other."),
        ]:
            changed = copy.deepcopy(self.variants)
            changed[0][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                reconstruct_catalog(changed, self.contract)

    def test_forged_export_and_matching_section_are_rejected(self):
        changed = copy.deepcopy(self.contract)
        name = changed["codeModeTools"][0]["name"]
        changed["codeModeTools"][0]["description"] += "\nOther instructions."
        changed["sections"][name] += "\nOther instructions."
        with self.assertRaises(ValueError):
            reconstruct_catalog(changed["codeModeTools"], changed)

    def test_version_one_does_not_admit_code_mode_variants(self):
        legacy = copy.deepcopy(self.contract)
        legacy["schemaVersion"] = 1
        del legacy["codeModeTools"]
        self.assertEqual(reconstruct_catalog(self.tools, legacy), [])
        with self.assertRaises(ValueError):
            reconstruct_catalog(self.variants, legacy)


if __name__ == "__main__":
    unittest.main()
