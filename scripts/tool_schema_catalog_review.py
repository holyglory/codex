"""Reproduce complete tool inventory blocks from bounded schema evidence only."""

import copy
import re

NAMES = {"account_management", "usage_stats", "usage_activity"}
HEADER = re.compile(
    r"(?P<prefix>(?:[0-9]{2,}:additional_tools/[^\s()]+ |  tools ))"
    r"\((?P<count>[0-9]+); hash=(?P<hash>[a-f0-9]{16})\):\n"
)


def reconstruct_catalog(current, contract):
    tools = contract.get("tools", [])
    sections = contract.get("sections", {})
    reviewed = {tool.get("name"): tool for tool in tools}
    if (
        len(tools) != 3
        or set(reviewed) != NAMES
        or set(sections) != NAMES
        or any(tool.get("type") != "function" for tool in tools)
        or any(not isinstance(section, str) for section in sections.values())
    ):
        raise ValueError("contract must contain exactly the three approved tools")

    def visit(catalog):
        if not isinstance(catalog, list):
            raise ValueError("catalog must be an array")
        result = []
        for tool in catalog:
            if tool.get("name") in NAMES:
                if tool != reviewed[tool["name"]]:
                    raise ValueError("direct tool differs from the approved contract")
                continue
            tool = copy.deepcopy(tool)
            if tool.get("type") == "namespace" and tool.get("name") == "functions":
                tool["tools"] = visit(tool["tools"])
            elif tool.get("type") == "custom" and tool.get("name") == "exec":
                text = tool["description"]
                for name in sorted(NAMES):
                    heading = f"### `{name}`\n"
                    if heading not in text:
                        continue
                    start = text.index(heading)
                    if text.count(heading) != 1 or (
                        start and not text[:start].endswith("\n\n")
                    ):
                        raise ValueError(
                            "approved section is duplicated or not anchored"
                        )
                    ends = [
                        text.find(separator, start)
                        for separator in ("\n\n### `", "\n\n## ")
                    ]
                    end = min(
                        (index for index in ends if index >= 0), default=len(text)
                    )
                    if text[start:end] != sections[name]:
                        raise ValueError(
                            "embedded section differs from the approved contract"
                        )
                    text = (
                        text[:start] + text[end + 2 :]
                        if end < len(text)
                        else text[: max(0, start - 2)]
                    )
                tool["description"] = text
            result.append(tool)
        return result

    return visit(current)


def tool_label(tool):
    definition = tool.get("function", tool)
    name = definition.get("name", tool.get("server_label"))
    kind = tool.get("type", "unknown")
    return f"{kind}/{name}" if isinstance(name, str) else kind


def render_tool(tool, fingerprint):
    definition = tool.get("function", tool)
    rendered = f"    - {tool_label(tool)}"
    description = definition.get("description")
    if isinstance(description, str) and description:
        description = description.replace("\n", " ")
        rendered += ": " + (
            description[:97] + "..." if len(description) > 100 else description
        )
    properties = definition.get("parameters", {}).get("properties")
    if isinstance(properties, dict):
        rendered += "; args=[" + ", ".join(sorted(properties)) + "]"
    rendered += f"; hash={fingerprint(tool)}"
    if isinstance(definition.get("tools"), list):
        for member in definition["tools"]:
            rendered += f"\n      - {tool_label(member)}"
    return rendered


def blocks(lines):
    result = []
    index = 0
    while index < len(lines):
        header = HEADER.fullmatch(lines[index])
        if header is None:
            result.append((None, lines[index]))
            index += 1
            continue
        indent = 4 if header["prefix"] == "  tools " else 8
        end = index + 1
        row = re.compile(rf"(?: {{{indent}}}[-+~] | {{{indent + 2}}}- )")
        while end < len(lines) and row.match(lines[end]):
            end += 1
        result.append((header, "".join(lines[index:end])))
        index = end
    return result


def render_block(header, catalog, fingerprint):
    rendered = header["prefix"] + f"({len(catalog)}; hash={fingerprint(catalog)}):\n"
    indent = "" if header["prefix"] == "  tools " else "    "
    for tool in catalog:
        rendered += (
            "\n".join(
                indent + line for line in render_tool(tool, fingerprint).split("\n")
            )
            + "\n"
        )
    return rendered


def review_body(before, after, catalogs, fingerprint):
    original, candidate = blocks(before), blocks(after)
    if len(original) != len(candidate):
        raise ValueError(
            "candidate changes non-tool structure or an unsupported inventory"
        )
    used = {}
    for (old_header, old), (new_header, new) in zip(original, candidate):
        if old == new:
            continue
        if (
            old_header is None
            or new_header is None
            or old_header["prefix"] != new_header["prefix"]
        ):
            raise ValueError("candidate changes non-tool text")
        pair = catalogs.get(old_header["hash"])
        if pair is None:
            raise ValueError("changed catalog has no exact evidence mapping")
        previous, current = pair
        if old != render_block(old_header, previous, fingerprint):
            raise ValueError(
                "inventory is not a proved full catalog; capture its predecessor before reviewing a delta"
            )
        if new != render_block(old_header, current, fingerprint):
            raise ValueError("candidate inventory differs from exact schema evidence")
        key = "catalog", old_header["hash"]
        used[key] = used.get(key, 0) + 1
    return "".join(after), used
