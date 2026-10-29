"""view.py SCENARIO [--errors] [--steps 3,5-7] [--full] : pair scenario steps with Java golden responses."""
import re, json, os, sys
root = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "tests", "conformance")
args = sys.argv[1:]; name = args[0]
errors_only = "--errors" in args; full = "--full" in args
steps_sel = None
if "--steps" in args:
    steps_sel = set()
    for part in args[args.index("--steps")+1].split(","):
        a, _, b = part.partition("-"); steps_sel.update(range(int(a), int(b or a)+1))
steps = []
for line in open(f"{root}/scenarios/{name}.scn").read().splitlines():
    if line.startswith("#"): continue
    if re.match(r"^(GET|POST|PUT|PATCH|DELETE|OPTIONS|HEAD) ", line): steps.append([line, ""])
    elif steps and not line.startswith("> "): steps[-1][1] += line
parts = open(f"{root}/java/{name}.golden").read().split("\n<<< END\n")[:-1]
def compact(s):
    s = re.sub(r'\{"property": \{"name": "__key__"\}\}', "KEY", s)
    s = re.sub(r'"partitionId": \{"projectId": "p"\}, ', "", s)
    return re.sub(r"\s+", " ", s)
for i, ((req, body), resp) in enumerate(zip(steps, parts), 1):
    if steps_sel and i not in steps_sel: continue
    status = resp.split("\n")[1][4:]; rbody = resp.split("\n", 2)[2]
    if errors_only and status == "200": continue
    if status != "200":
        try: out = json.loads(rbody)["error"].get("message", "<no message>")
        except Exception: out = rbody.strip()
    elif full: out = rbody
    else:
        try:
            d = json.loads(rbody); b = d.get("batch", d)
            keys = [ "/".join(f"{e['kind']}:{e.get('name', e.get('id'))}" for e in r["entity"]["key"]["path"]) for r in b.get("entityResults", [])]
            extra = {k: v for k, v in b.items() if k not in ("entityResults",)}
            out = f"{keys} {json.dumps(extra)[:300]}"
        except Exception: out = compact(rbody)[:300]
    print(f"[{i}] {req.split(' ',1)[1] if not req.endswith(':runQuery') else ''}{compact(body)[:220]}\n    -> {status} {out}")
