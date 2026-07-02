# Advanced fixed headless flow — multi-step + pagination + error handling.
# NO LLM. Activate with NEVOFLUX_HEADLESS_SCRIPT=/opt/nevoflux/fixed-flow-advanced.py
#
# run(task) treats `task` as a search query, scrapes up to MAX_PAGES of results,
# and ALWAYS returns a structured dict — it never lets an exception reach the
# interface. Shape:
#   success -> {"ok": True,  "query": task, "pages": N, "items": [...]}
#   failure -> {"ok": False, "error": "...", "query": task}
#
# ---- Monty (sandboxed Python) syntax — verified supported ----
#   def / if-elif-else / for-while / try-except / comprehensions / f-strings /
#   lambda / slicing / isinstance / dict.get / f-strings.
#   `import json/re/time/math/...` is fine (auto-handled). time.sleep works.
#   NOT supported: class, match/case, with, async/await, yield, decorators,
#   map()/filter() (use comprehensions), sorted(key=/reverse=).
#
# ---- Tool conventions (verified live) ----
#   * browser_navigate opens a NEW, INACTIVE tab -> thread nav["tab_id"] everywhere.
#   * Tool errors come back as a dict {"__tool_error": True, "error": "..."} —
#     they do NOT raise; check with is_err() below.
#   * browser_get_markdown(tab_id=...) -> {"markdown","title","url","success"}.

MAX_PAGES = 5


def is_err(r):
    """True if a tool call returned an error envelope instead of a result."""
    return isinstance(r, dict) and r.get("__tool_error")


def _scrape(task):
    # 1) open the search site (keep the new tab's id)
    nav = browser_navigate(url="https://example.com/search")
    if is_err(nav):
        return {"ok": False, "query": task, "error": "navigate failed: " + str(nav.get("error"))}
    tab = nav["tab_id"]

    # 2) type the query and submit
    fill = browser_fill(selector="#q", value=task, tab_id=tab)
    if is_err(fill):
        return {"ok": False, "query": task, "error": "search box #q not found"}
    browser_click(selector="button[type=submit]", tab_id=tab)

    # 3) wait for the first results page to render
    ready = browser_wait_for(selector="#results", tab_id=tab, timeout_ms=15000)
    if is_err(ready):
        return {"ok": False, "query": task, "error": "results never rendered"}

    # 4) collect page by page; stop when a page fails or there's no 'next'
    items = []
    for page in range(MAX_PAGES):
        md = browser_get_markdown(tab_id=tab)
        if is_err(md):
            break
        items.append({"page": page + 1, "url": md.get("url"), "markdown": md.get("markdown")})

        # advance to the next page if a 'next' link exists (short wait = probe)
        nxt = browser_wait_for(selector="a.next", tab_id=tab, timeout_ms=3000)
        if is_err(nxt):
            break  # no next link -> done
        if is_err(browser_click(selector="a.next", tab_id=tab)):
            break
        browser_wait_for(selector="#results", tab_id=tab, timeout_ms=15000)

    if not items:
        return {"ok": False, "query": task, "error": "no results collected"}
    return {"ok": True, "query": task, "pages": len(items), "items": items}


def run(task):
    # Wrap so ANY unexpected Python error is returned structured, not raised
    # (a raised exception would surface as the task's `error` with status failed;
    # returning it lets the caller always parse a JSON body).
    try:
        return _scrape(task)
    except Exception as e:
        return {"ok": False, "query": task, "error": "script error: " + str(e)}
