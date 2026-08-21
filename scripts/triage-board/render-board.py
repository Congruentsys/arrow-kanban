#!/usr/bin/env python3
"""Render an interactive triage board (HTML + CSV) from an arrow-kanban export.

Input:  a JSON file produced by `<client> export --format json` (an array of items).
Output: board.html — a self-contained page: rank strip, filterable/sortable table,
        voyage hierarchy, per-item notes (browser-local) with a copy-out button.
        board.csv — the same rows for spreadsheet use.

Ranking is data-driven: pass --ranks "tag-a,tag-b,…" (highest first). An item's
campaign is its first matching rank tag; items matching none group under
"(unranked)" at the end, so the tool works with zero configuration too.
"""
import argparse, csv, html, json, sys
from collections import Counter

TERMINAL = {"done", "complete", "completed", "abandoned", "retired", "obsolete"}
CONTAINERS = {"voyage", "campaign"}
PRI = {"critical": 0, "high": 1, "medium": 2, "low": 3, "": 4}


def load_items(path):
    items = json.load(open(path))
    seen, out = set(), []
    for it in items:
        if it["id"] in seen:
            continue
        seen.add(it["id"])
        if (it.get("status") or "").lower() in TERMINAL:
            continue
        out.append(it)
    return out


def build_rows(items, ranks):
    def campaign(tags):
        for t in ranks:
            if t in tags:
                return t
        return "(unranked)"

    groups = {t: [] for t in ranks + ["(unranked)"]}
    for it in items:
        tags = set(it.get("tags") or [])
        camp = campaign(tags)
        if camp == "(unranked)" and ranks and not tags:
            continue
        groups.setdefault(camp, []).append(it)

    rows = []
    for pos, camp in enumerate(list(ranks) + ["(unranked)"]):
        g = groups.get(camp) or []
        ids = {x["id"] for x in g}
        for x in g:
            x["_parent"] = ""
            if x.get("type") not in CONTAINERS:
                for rel in (x.get("related") or []) + (x.get("depends_on") or []):
                    if isinstance(rel, str) and rel in ids:
                        target = next(y for y in g if y["id"] == rel)
                        if target.get("type") in CONTAINERS:
                            x["_parent"] = rel
                            break
        key = lambda x: (PRI.get((x.get("priority") or "").lower(), 4), x["id"])
        containers = sorted([x for x in g if x.get("type") in CONTAINERS], key=key)
        used, ordered = set(), []
        for v in containers:
            ordered.append(v)
            used.add(v["id"])
            for k in sorted([x for x in g if x.get("_parent") == v["id"]], key=key):
                k["_child"] = True
                ordered.append(k)
                used.add(k["id"])
        ordered += sorted([x for x in g if x["id"] not in used], key=key)
        for x in ordered:
            rows.append({
                "rank": str(pos + 1) if camp != "(unranked)" else "—",
                "camp": camp, "id": x["id"], "parent": x.get("_parent", ""),
                "type": x.get("type", ""), "status": x.get("status", ""),
                "priority": (x.get("priority") or "").lower(),
                "assignee": x.get("assignee") or "",
                "depends_on": x.get("depends_on") or [],
                "tags": sorted(set(x.get("tags") or [])),
                "title": x.get("title", ""), "child": bool(x.get("_child")),
            })
    return rows


CSS_JS_PAGE = """<title>__TITLE__</title>
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Archivo:wght@500;600;700&display=swap">
<style>
:root{color-scheme:light;
 --bg:#f6f7f6;--panel:#fff;--ink:#161a19;--ink2:#4c5553;--mut:#79837f;--rule:#dde3e0;--rule2:#edf0ee;
 --acc:#0e7f74;--acc-ink:#fff;--crit:#b3382c;--chipbg:#eef3f1;--hl:#e7f2f0;
 --mono:ui-monospace,SFMono-Regular,Menlo,monospace;--disp:'Archivo',system-ui,sans-serif;--body:system-ui,sans-serif}
@media (prefers-color-scheme:dark){:root:not([data-theme="light"]){color-scheme:dark;
 --bg:#141817;--panel:#1b201f;--ink:#e8ecea;--ink2:#aab4b0;--mut:#7d8783;--rule:#2b3230;--rule2:#232928;
 --acc:#2aa79a;--acc-ink:#0c1211;--crit:#e0685c;--chipbg:#242b29;--hl:#1e2a28}}
:root[data-theme="dark"]{color-scheme:dark;
 --bg:#141817;--panel:#1b201f;--ink:#e8ecea;--ink2:#aab4b0;--mut:#7d8783;--rule:#2b3230;--rule2:#232928;
 --acc:#2aa79a;--acc-ink:#0c1211;--crit:#e0685c;--chipbg:#242b29;--hl:#1e2a28}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--ink);font:14px/1.5 var(--body)}
.wrap{max-width:1340px;margin:0 auto;padding:22px 20px 80px}
h1{font:700 24px/1.15 var(--disp);margin:0}
.sub{color:var(--ink2);margin:4px 0 14px;max-width:80ch}
.strip{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:8px;margin:12px 0 16px}
.rk{text-align:left;background:var(--panel);border:1px solid var(--rule);border-radius:6px;padding:9px 11px;
 cursor:pointer;color:var(--ink);display:flex;flex-direction:column;gap:2px;font-family:var(--body)}
.rk:hover{border-color:var(--acc)}.rk.on{border-color:var(--acc);background:var(--hl)}
.rknum{font:700 11px var(--mono);color:var(--acc)}.rkname{font:600 13px var(--disp);word-break:break-all}
.rkmeta{font:11px var(--mono);color:var(--mut)}
.bar{display:flex;gap:8px;flex-wrap:wrap;align-items:center;position:sticky;top:0;background:var(--bg);
 padding:8px 0;z-index:5;border-bottom:1px solid var(--rule)}
input[type=search],select{background:var(--panel);color:var(--ink);border:1px solid var(--rule);
 border-radius:5px;padding:6px 9px;font:13px var(--body)}
input[type=search]{min-width:230px}
.btn{background:var(--acc);color:var(--acc-ink);border:none;border-radius:5px;padding:7px 12px;
 font:600 12.5px var(--disp);cursor:pointer}
.btn.ghost{background:var(--panel);color:var(--ink);border:1px solid var(--rule)}
.count{font:12px var(--mono);color:var(--mut);margin-left:auto}
.tablewrap{overflow-x:auto;background:var(--panel);border:1px solid var(--rule);border-radius:6px;margin-top:12px}
table{border-collapse:collapse;width:100%;font-size:12.5px}
th,td{padding:6px 9px;text-align:left;vertical-align:top;border-top:1px solid var(--rule2)}
thead th{position:sticky;top:0;background:var(--panel);font:600 10.5px var(--disp);letter-spacing:.07em;
 text-transform:uppercase;color:var(--mut);border-bottom:1px solid var(--rule);cursor:pointer;white-space:nowrap}
tr.camphead td{background:var(--hl);font:700 13px var(--disp);border-top:2px solid var(--rule)}
td.id{font-family:var(--mono);white-space:nowrap}
td.id .cr{color:var(--crit);font-weight:700}
.child{color:var(--mut);font-family:var(--mono)}
.dep{font:11px var(--mono);color:var(--ink2)}
.tag{display:inline-block;background:var(--chipbg);border-radius:4px;padding:0 6px;
 font:11px var(--mono);color:var(--ink2);margin:1px 2px 1px 0}
td.cmt{min-width:170px}
.cmtbox{min-height:1.3em;border:1px dashed var(--rule);border-radius:4px;padding:3px 6px}
.cmtbox:focus{outline:2px solid var(--acc);border-style:solid}
.cmtbox:empty:before{content:"click to note…";color:var(--mut)}
.pri-critical{color:var(--crit);font-weight:700}.pri-high{color:var(--ink)}.pri-medium,.pri-low,.pri-{color:var(--mut)}
.st{font:11px var(--mono);color:var(--ink2);white-space:nowrap}
:focus-visible{outline:2px solid var(--acc);outline-offset:1px}
@media (prefers-reduced-motion:reduce){*{transition:none!important}}
</style>
<div class="wrap">
<h1>__TITLE__</h1>
<p class="sub">Every open item, grouped by rank (highest first). Click a rank card to filter;
click column headers to sort; notes persist in this browser and <b>Copy notes</b> puts
<code>ID → note</code> lines on the clipboard.</p>
<div class="strip">__STRIP__</div>
<div class="bar">
<input type="search" id="q" placeholder="search id / title / tags / deps…">
<select id="fp"><option value="">all priorities</option><option>critical</option><option>high</option>
<option>medium</option><option>low</option></select>
<select id="fs"><option value="">all statuses</option><option>backlog</option><option>in_progress</option>
<option>review</option><option>blocked</option><option>planning</option></select>
<button class="btn ghost" id="clear">Clear</button>
<button class="btn" id="copy">Copy notes</button>
<span class="count" id="count"></span>
</div>
<div class="tablewrap"><table><thead><tr>
<th data-k="rank">Rank</th><th data-k="id">Item</th><th data-k="type">Type</th><th data-k="status">Status</th>
<th data-k="priority">Pri</th><th data-k="parent">Parent</th><th>Depends on</th><th data-k="title">Title</th>
<th>Tags</th><th>Notes</th>
</tr></thead><tbody id="tb"></tbody></table></div>
</div>
<script>
const DATA=__DATA__;
const tb=document.getElementById('tb');let camp="",sortk="",sortdir=1;
function esc(s){return String(s).replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]))}
function store(){try{return JSON.parse(localStorage.getItem('board-notes')||'{}')}catch(e){return{}}}
function saveN(id,v){try{const s=store();if(v)s[id]=v;else delete s[id];localStorage.setItem('board-notes',JSON.stringify(s))}catch(e){}}
function render(){
 const q=document.getElementById('q').value.toLowerCase(),
 fp=document.getElementById('fp').value,fs=document.getElementById('fs').value;
 let rows=DATA.filter(r=>(!camp||r.camp===camp)&&(!fp||r.priority===fp)&&(!fs||r.status===fs)
  &&(!q||(r.id+' '+r.title+' '+r.tags.join(' ')+' '+r.parent+' '+r.depends_on.join(' ')).toLowerCase().includes(q)));
 if(sortk){rows=[...rows].sort((a,b)=>{const A=String(a[sortk]??''),B=String(b[sortk]??'');
  return A<B?-sortdir:A>B?sortdir:0})}
 const cm=store();let out='',last='';
 for(const r of rows){
  if(!sortk&&r.camp!==last){last=r.camp;
   out+=`<tr class="camphead"><td colspan="10">rank ${esc(r.rank)} — ${esc(r.camp)}</td></tr>`}
  out+=`<tr><td>${esc(r.rank)}</td>
  <td class="id">${r.child?'<span class="child">└ </span>':''}<span class="${r.priority==='critical'?'cr':''}">${esc(r.id)}</span></td>
  <td class="st">${esc(r.type)}</td><td class="st">${esc(r.status)}</td>
  <td class="pri-${esc(r.priority)}">${esc(r.priority||'—')}</td>
  <td class="dep">${esc(r.parent)}</td><td class="dep">${esc(r.depends_on.join(' '))}</td>
  <td>${esc(r.title)}</td>
  <td>${r.tags.map(t=>`<span class="tag">${esc(t)}</span>`).join('')}</td>
  <td class="cmt"><div class="cmtbox" contenteditable="true" data-id="${esc(r.id)}">${esc(cm[r.id]||'')}</div></td></tr>`}
 tb.innerHTML=out;
 document.getElementById('count').textContent=rows.length+' / '+DATA.length+' items';
 tb.querySelectorAll('.cmtbox').forEach(b=>b.addEventListener('input',()=>saveN(b.dataset.id,b.textContent.trim())));
}
document.querySelectorAll('.rk').forEach(b=>b.addEventListener('click',()=>{
 camp=camp===b.dataset.camp?'':b.dataset.camp;
 document.querySelectorAll('.rk').forEach(x=>x.classList.toggle('on',x.dataset.camp===camp));render()}));
['q','fp','fs'].forEach(id=>document.getElementById(id).addEventListener('input',render));
document.getElementById('clear').addEventListener('click',()=>{camp='';sortk='';
 document.querySelectorAll('.rk').forEach(x=>x.classList.remove('on'));
 ['q','fp','fs'].forEach(id=>document.getElementById(id).value='');render()});
document.querySelectorAll('thead th[data-k]').forEach(th=>th.addEventListener('click',()=>{
 const k=th.dataset.k;if(sortk===k)sortdir*=-1;else{sortk=k;sortdir=1}render()}));
document.getElementById('copy').addEventListener('click',()=>{
 const s=store();const lines=Object.entries(s).filter(([,v])=>v).map(([k,v])=>k+' → '+v);
 navigator.clipboard.writeText(lines.length?lines.join('\\n'):'(no notes yet)').then(()=>{
  const b=document.getElementById('copy');b.textContent='Copied '+lines.length+' notes';
  setTimeout(()=>b.textContent='Copy notes',1800)})});
render();
</script>"""


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("export_json")
    ap.add_argument("out_dir")
    ap.add_argument("--ranks", default="", help="comma-separated tags, highest rank first")
    ap.add_argument("--title", default="Triage Board")
    a = ap.parse_args()
    ranks = [t.strip() for t in a.ranks.split(",") if t.strip()]
    rows = build_rows(load_items(a.export_json), ranks)

    with open(f"{a.out_dir}/board.csv", "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["rank", "campaign", "id", "parent", "type", "status", "priority",
                    "assignee", "depends_on", "tags", "title", "notes"])
        for r in rows:
            w.writerow([r["rank"], r["camp"], r["id"], r["parent"], r["type"], r["status"],
                        r["priority"], r["assignee"], " ".join(r["depends_on"]),
                        " ".join(r["tags"]), r["title"][:160], ""])

    counts = Counter(r["camp"] for r in rows)
    crit = Counter(r["camp"] for r in rows if r["priority"] == "critical")
    seen_order, strip = [], ""
    for r in rows:
        if r["camp"] not in seen_order:
            seen_order.append(r["camp"])
    for c in seen_order:
        rank = next(r["rank"] for r in rows if r["camp"] == c)
        strip += (f'<button class="rk" data-camp="{html.escape(c)}"><span class="rknum">{rank}</span>'
                  f'<span class="rkname">{html.escape(c)}</span>'
                  f'<span class="rkmeta">{counts[c]} open · {crit[c]} crit</span></button>')

    page = (CSS_JS_PAGE.replace("__TITLE__", html.escape(a.title))
            .replace("__STRIP__", strip).replace("__DATA__", json.dumps(
                [{k: r[k] for k in ("rank", "camp", "id", "parent", "type", "status",
                                    "priority", "depends_on", "tags", "title", "child")} for r in rows])))
    open(f"{a.out_dir}/board.html", "w").write(page)
    print(f"rendered {len(rows)} rows -> {a.out_dir}/board.html + board.csv")


if __name__ == "__main__":
    main()
