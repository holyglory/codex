"""Independently verify the three approved Code Mode description variants.

This reproduces only the primitive/enum object schemas used by the production
contract. Unsupported schema shapes remain rejected rather than approximated.
The source rules are in code-mode-protocol/src/{description,json_schema_types}.rs.
"""

import copy
import json
import re

NAMES = {"account_management", "usage_stats", "usage_activity"}
IDENTIFIER = re.compile(r"[A-Za-z_$][A-Za-z0-9_$]*\Z")
MIN_SCHEMA_BUDGET = 16_000


def code_mode_description(tool):
    name = tool.get("name")
    schema = tool.get("parameters", {})
    if (
        name not in NAMES
        or tool.get("type") != "function"
        or not isinstance(tool.get("description"), str)
        or tool.get("output_schema") is not None
        or set(schema) != {"type", "properties", "required", "additionalProperties"}
        or schema.get("type") != "object"
        or schema.get("additionalProperties") is not False
        or not isinstance(schema.get("properties"), dict)
        or not isinstance(schema.get("required"), list)
    ):
        raise ValueError("unsupported approved Code Mode schema shape")
    properties = schema["properties"]
    required = schema["required"]
    if (
        len(properties) > 32
        or any(not isinstance(value, dict) for value in properties.values())
        or any(not isinstance(key, str) or key not in properties for key in required)
    ):
        raise ValueError("unsupported approved Code Mode property inventory")
    multiline = any(value.get("description") for value in properties.values())
    lines = []
    enum_bytes = 0
    for key, value in sorted(properties.items()):
        if not IDENTIFIER.fullmatch(key) or set(value) - {
            "type",
            "enum",
            "description",
        }:
            raise ValueError("unsupported approved Code Mode property")
        description = value.get("description", "")
        if not isinstance(description, str):
            raise ValueError("unsupported approved Code Mode property description")
        if multiline:
            lines.extend(
                "  // " + line.strip()
                for line in description.splitlines()
                if line.strip()
            )
        if "enum" in value:
            values = value["enum"]
            if (
                value.get("type") != "string"
                or not isinstance(values, list)
                or not values
                or any(not isinstance(item, str) for item in values)
            ):
                raise ValueError("unsupported approved Code Mode enum")
            literals = [json.dumps(item, ensure_ascii=False) for item in values]
            rendered_type = " | ".join(literals)
            enum_bytes += sum(len(item.encode()) for item in literals)
        else:
            rendered_type = {
                "string": "string",
                "integer": "number",
                "number": "number",
                "boolean": "boolean",
            }.get(value.get("type"))
            if rendered_type is None:
                raise ValueError("unsupported approved Code Mode primitive")
        optional = "" if key in required else "?"
        lines.append(("  " if multiline else "") + f"{key}{optional}: {rendered_type};")
    if multiline:
        input_type = "{\n" + "\n".join(lines) + "\n}"
    else:
        input_type = "{ " + " ".join(lines) + " }" if lines else "{}"
    # This profile has no refs, nested schemas or intersections. This upper
    # bound covers literal, property-line and complete-object render work.
    input_bytes = len(input_type.encode())
    if (
        input_bytes > MIN_SCHEMA_BUDGET
        or 4 * input_bytes + 3 * enum_bytes > 4 * MIN_SCHEMA_BUDGET
    ):
        raise ValueError(
            "approved Code Mode declaration exceeds its source rendering budget"
        )
    return (
        tool["description"]
        + "\n\nexec tool declaration:\n```ts\n"
        + f"declare const tools: {{ {name}(args: {input_type}): Promise<unknown>; }};\n```"
    )


def approved_code_mode_tools(contract):
    version = contract.get("schemaVersion", 1)
    if not isinstance(version, int) or isinstance(version, bool):
        raise ValueError("invalid schema contract version")
    if version == 1:
        if "codeModeTools" in contract:
            raise ValueError("Code Mode variants require contract version 2")
        return {}
    if version != 2:
        raise ValueError("unsupported schema contract version")
    variants = contract.get("codeModeTools")
    if not isinstance(variants, list) or len(variants) != 3:
        raise ValueError("contract version 2 requires exactly three Code Mode variants")
    by_name = {tool.get("name"): tool for tool in variants}
    if set(by_name) != NAMES:
        raise ValueError("Code Mode variant identities differ from approved tools")
    for tool in contract["tools"]:
        expected = copy.deepcopy(tool)
        expected["description"] = code_mode_description(tool)
        if by_name[tool["name"]] != expected:
            raise ValueError(
                "Code Mode variant differs from the exact production transformation"
            )
        expected_section = f"### `{tool['name']}`\n" + expected["description"].strip()
        if contract["sections"][tool["name"]] != expected_section:
            raise ValueError(
                "Code Mode section differs from the independent declaration renderer"
            )
    return by_name
