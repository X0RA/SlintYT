#!/usr/bin/env python3
"""Drive the running app through Slint's embedded MCP server.

Start the app first (see AGENTS.md), then for example:

    ./slint-mcp.py find --type RailItem
    ./slint-mcp.py click --type RailItem --label Twitch
    ./slint-mcp.py type --id SearchBar::search-field "shroud" --enter
    ./slint-mcp.py wait --id TwitchResultRow::touch --wait 20
    ./slint-mcp.py props --id VideoPlayer::seek-bar --nth 0
    ./slint-mcp.py texts --contains bookmark
    ./slint-mcp.py call get_element_tree '{"elementHandle":{"generation":"1","index":"1"},"maxElements":50}'

Elements are picked with --id (exact element ID), --type (type name or base,
e.g. RailItem, Text, Image) and --label (substring of the accessible label,
case-insensitive), then --nth to choose one of several matches (default 0).
Standard library only.
"""

import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.request

URL = f"http://127.0.0.1:{os.environ.get('SLINT_MCP_PORT', '9315')}/mcp"


def call(name, **arguments):
    body = json.dumps(
        {"jsonrpc": "2.0", "id": 1, "method": "tools/call",
         "params": {"name": name, "arguments": arguments}}
    ).encode()
    request = urllib.request.Request(URL, body, {"Content-Type": "application/json"})
    try:
        reply = json.load(urllib.request.urlopen(request, timeout=30))
    except urllib.error.URLError as error:
        sys.exit(f"Could not reach {URL}: {error.reason}. Is the app running with --features slint/mcp?")
    if "error" in reply:
        sys.exit(f"{name}: {reply['error']}")
    result = reply["result"]
    text = result["content"][0]["text"] if result.get("content") else ""
    if result.get("isError"):
        sys.exit(f"{name}: {text}")
    try:
        return json.loads(text)
    except ValueError:
        return text


def window():
    handles = call("list_windows").get("windowHandles", [])
    if not handles:
        sys.exit("The app has no windows yet.")
    return handles[0]


def root():
    return call("get_window_properties", windowHandle=window())["rootElementHandle"]


def props(handle):
    return call("get_element_properties", elementHandle=handle)


def summary(handle):
    """One line per element: handle, ids, label, value, position and size."""
    p = props(handle)
    names = [n.get("id") or n.get("typeName") for n in p.get("typeNamesAndIds", []) if n]
    position, size = p.get("absolutePosition", {}), p.get("size", {})
    out = {
        "handle": handle["index"],
        "ids": names,
        "label": p.get("accessibleLabel"),
        "value": p.get("accessibleValue"),
        "at": [round(position.get("x", 0)), round(position.get("y", 0))],
        "size": [round(size.get("width", 0)), round(size.get("height", 0))],
    }
    if p.get("accessibleDescription"):
        out["description"] = p["accessibleDescription"]
    return {k: v for k, v in out.items() if v not in (None, [])}


def matches(args):
    if args.id:
        handles = call("find_elements_by_id", windowHandle=window(), elementsId=args.id).get("elementHandles", [])
    elif args.type:
        handles = call(
            "query_element_descendants", elementHandle=root(),
            queryStack=[{"matchElementTypeNameOrBase": args.type}], findAll=True,
        ).get("elementHandles", [])
    else:
        sys.exit("Pick elements with --id or --type.")
    if args.label:
        wanted = args.label.lower()
        handles = [h for h in handles if wanted in (props(h).get("accessibleLabel") or "").lower()]
    return handles


def one(args):
    handles = matches(args)
    if args.nth >= len(handles):
        sys.exit(f"No match #{args.nth}: found {len(handles)} element(s).")
    return handles[args.nth]


def wait_for(args, timeout):
    deadline = time.monotonic() + timeout
    while True:
        handles = matches(args)
        if len(handles) > args.nth:
            return handles
        if time.monotonic() >= deadline:
            sys.exit(f"Timed out after {timeout}s waiting for the element.")
        time.sleep(0.5)


def show(value):
    print(json.dumps(value, ensure_ascii=False))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)

    def selector(command):
        command.add_argument("--id", help="exact element ID, e.g. VideoPlayer::seek-bar")
        command.add_argument("--type", help="type name or base, e.g. RailItem")
        command.add_argument("--label", help="substring of the accessible label")
        command.add_argument("--nth", type=int, default=0, help="which match to use")
        command.add_argument("--wait", type=float, default=0, help="seconds to wait for a match")
        return command

    selector(commands.add_parser("find", help="list matching elements"))
    selector(commands.add_parser("props", help="all properties of one element"))
    selector(commands.add_parser("click", help="click an element")).add_argument(
        "--right", action="store_true", help="right-click")
    selector(commands.add_parser("hover", help="hover an element"))
    typing = selector(commands.add_parser("type", help="set a text field's value"))
    typing.add_argument("text")
    typing.add_argument("--enter", action="store_true", help="press Enter afterwards")
    selector(commands.add_parser("wait", help="wait until an element exists")).set_defaults(wait=15)
    texts = commands.add_parser("texts", help="list visible text")
    texts.add_argument("--contains", help="case-insensitive filter")
    key = commands.add_parser("key", help="press and release a key, e.g. '\\n' or ' '")
    key.add_argument("text")
    pointer = commands.add_parser("pointer", help="move the pointer, e.g. to reveal auto-hidden controls")
    pointer.add_argument("x", type=float)
    pointer.add_argument("y", type=float)
    tree = commands.add_parser("tree", help="the element tree under the root")
    tree.add_argument("--max", type=int, default=50)
    raw = commands.add_parser("call", help="call any MCP tool with JSON arguments")
    raw.add_argument("tool")
    raw.add_argument("arguments", nargs="?", default="{}")
    commands.add_parser("window", help="window handle, size and root element")
    args = parser.parse_args()

    if args.command == "call":
        show(call(args.tool, **json.loads(args.arguments)))
    elif args.command == "window":
        handle = window()
        show({"window": handle, **call("get_window_properties", windowHandle=handle)})
    elif args.command == "tree":
        show(call("get_element_tree", elementHandle=root(), maxElements=args.max))
    elif args.command == "texts":
        args.id, args.type, args.label = None, "Text", args.contains
        for handle in matches(args):
            show(summary(handle))
    elif args.command == "key":
        call("dispatch_key_event", windowHandle=window(), eventType="PressAndRelease",
             text=args.text.encode().decode("unicode_escape"))
    elif args.command == "pointer":
        call("move_pointer", windowHandle=window(), position={"x": args.x, "y": args.y})
    else:
        handles = wait_for(args, args.wait) if args.wait else matches(args)
        if args.command in ("find", "wait"):
            for handle in handles:
                show(summary(handle))
            return
        handle = one(args)
        if args.command == "props":
            show(props(handle))
        elif args.command == "click":
            call("click_element", elementHandle=handle, button="Right" if args.right else "Left")
        elif args.command == "hover":
            call("hover_element", elementHandle=handle)
        elif args.command == "type":
            call("set_element_value", elementHandle=handle, value=args.text)
            if args.enter:
                call("click_element", elementHandle=handle)
                call("dispatch_key_event", windowHandle=window(), eventType="PressAndRelease", text="\n")


if __name__ == "__main__":
    main()
