"""Reproduce full inventories and settings deltas from exact schema evidence."""

import copy
import re

NAMES = {"account_management", "usage_stats", "usage_activity"}
HEADER = re.compile(
    r"(?P<prefix>(?:[0-9]{2,}:additional_tools/[^\s()]+ |  tools ))"
    r"\((?P<count>[0-9]+); hash=(?P<hash>[a-f0-9]{16})\):\n"
)
WINDOW = re.compile(r"## Window (?P<number>[1-9][0-9]*)(?: \([^\n]*\))?\n")
RELATIVE = re.compile(r"Settings: relative to window (?P<number>[1-9][0-9]*)\n")
SAME = re.compile(r"Settings: same as window (?P<number>[1-9][0-9]*)\n")
UNAVAILABLE = object()
UNKNOWN = object()


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


def render_tool(tool, fingerprint, marker="-"):
    definition = tool.get("function", tool)
    rendered = f"    {marker} {tool_label(tool)}"
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


def render_tools(catalog, fingerprint, previous=None):
    if previous is None:
        return [render_tool(tool, fingerprint) for tool in catalog]
    old_labels = [tool_label(tool) for tool in previous]
    new_labels = [tool_label(tool) for tool in catalog]
    if len(set(old_labels)) != len(old_labels) or len(set(new_labels)) != len(
        new_labels
    ):
        return ["    - inventory changed (ambiguous tool names)"]
    changes = [
        f"    - removed {label}" for label in old_labels if label not in new_labels
    ]
    old_by_label = dict(zip(old_labels, previous))
    for label, tool in zip(new_labels, catalog):
        if label not in old_by_label:
            changes.append(render_tool(tool, fingerprint, "+"))
        elif old_by_label[label] != tool:
            changes.append(render_tool(tool, fingerprint, "~"))
    return changes or ["    - order changed"]


def render_block(header, catalog, fingerprint, previous=None):
    rendered = header["prefix"] + f"({len(catalog)}; hash={fingerprint(catalog)}):\n"
    indent = "" if header["prefix"] == "  tools " else "    "
    for tool in render_tools(catalog, fingerprint, previous):
        rendered += "\n".join(indent + line for line in tool.split("\n")) + "\n"
    return rendered


def review_body(before, after, catalogs, fingerprint):
    original, candidate = blocks(before), blocks(after)
    if len(original) != len(candidate):
        raise ValueError(
            "candidate changes non-tool structure or an unsupported inventory"
        )
    used = {}
    windows = {}
    window = 0
    mode = None
    prior = None
    tools_seen = False
    for (old_header, old), (new_header, new) in zip(original, candidate):
        if old_header is None:
            if old != new:
                raise ValueError("candidate changes non-tool text")
            heading = WINDOW.fullmatch(old)
            relative, same = RELATIVE.fullmatch(old), SAME.fullmatch(old)
            if heading:
                window = int(heading["number"])
                if window != len(windows) + 1:
                    raise ValueError("unsupported settings window sequence")
                windows[window] = UNAVAILABLE
                mode = None
                tools_seen = False
            elif old.startswith("Settings:"):
                if not window or mode is not None:
                    raise ValueError("unsupported settings lineage")
                if old == "Settings:\n":
                    if windows.get(window - 1, UNAVAILABLE) is not UNAVAILABLE:
                        raise ValueError("full settings have an available predecessor")
                    mode, prior = "full", None
                    windows[window] = None
                elif relative:
                    target = int(relative["number"])
                    if (
                        target != window - 1
                        or windows.get(target, UNAVAILABLE) is UNAVAILABLE
                    ):
                        raise ValueError(
                            "settings delta has no available preceding window"
                        )
                    mode, prior = "delta", windows[target]
                    windows[window] = prior
                elif same:
                    target = int(same["number"])
                    if (
                        target >= window
                        or windows.get(target, UNAVAILABLE) is UNAVAILABLE
                    ):
                        raise ValueError(
                            "settings reuse has no available earlier window"
                        )
                    mode = "same"
                    windows[window] = windows[target]
                elif old == "Settings: unavailable (items only)\n":
                    mode = "unavailable"
                else:
                    raise ValueError("unsupported settings lineage")
            elif old.startswith("-- request "):
                mode = None
            elif mode in ("full", "delta") and old.startswith("  tools:"):
                if old != "  tools: <removed>\n" or mode != "delta" or tools_seen:
                    raise ValueError("unsupported non-array tools setting")
                windows[window] = None
                tools_seen = True
            continue
        if new_header is None or old_header["prefix"] != new_header["prefix"]:
            raise ValueError("candidate changes non-tool structure")
        settings = old_header["prefix"] == "  tools "
        if settings and (mode not in ("full", "delta") or tools_seen):
            raise ValueError("tool delta has no supported settings lineage")
        predecessor = prior if settings and mode == "delta" else None
        if predecessor is UNKNOWN:
            raise ValueError("settings predecessor has no exact catalog preimage")
        pair = catalogs.get(old_header["hash"])
        if pair is None:
            if settings and old == new:
                windows[window] = UNKNOWN
                tools_seen = True
                continue
            if old == new:
                continue
            raise ValueError("changed catalog has no exact evidence mapping")
        previous, current = pair
        old_prior, new_prior = predecessor if predecessor is not None else (None, None)
        if old != render_block(old_header, previous, fingerprint, old_prior):
            raise ValueError(
                "original inventory differs from exact schema evidence or lineage"
            )
        if new != render_block(old_header, current, fingerprint, new_prior):
            raise ValueError("candidate inventory differs from exact schema evidence")
        if settings:
            windows[window] = pair
            tools_seen = True
        if old != new:
            key = "catalog", old_header["hash"]
            used[key] = used.get(key, 0) + 1
    return "".join(after), used
