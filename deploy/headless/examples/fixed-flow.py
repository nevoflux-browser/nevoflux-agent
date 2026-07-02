# Fixed headless browser-use flow — NO LLM required.
#
# Activate by pointing the daemon at this file:
#   NEVOFLUX_HEADLESS_SCRIPT=/opt/nevoflux/fixed-flow.py
# When set, every headless task runs THIS script instead of the LLM agent loop.
#
# The daemon calls run(task) with the interface's task string:
#   - POST /tasks            -> the "task" field
#   - OpenAI /v1/chat/...    -> the last user message
#   - MCP run_browser_task   -> the "task" argument
#   - ACP session/prompt     -> the joined prompt text
# Whatever run(task) RETURNS (or, failing that, prints) becomes the interface
# `output`. A raised exception -> the task reports status "failed" + the error.
#
# ---- Two gotchas verified live ----------------------------------------------
# 1) browser_navigate opens a NEW, INACTIVE tab and returns {"tab_id": N, ...}.
#    Tools default to the *active* tab, so you MUST thread that tab_id into every
#    later call, or you get "No active web tab found".
# 2) Tool results are STRUCTURED (dicts/JSON), not plain strings. e.g.
#    browser_get_markdown(...) -> {"markdown": "...", "title": "...",
#    "url": "...", "success": true}. Index the field you want (["markdown"]).
#
# Available tools = the browser tools, called by keyword. Common ones
# (tab_id is optional but recommended here — see gotcha #1):
#   browser_navigate(url=...)                        -> {tab_id, url, new_tab}
#   browser_fill(selector=..., value=..., tab_id=...)      # set an input's value
#   browser_input(selector=..., text=..., tab_id=...)      # high-level text input
#   browser_click(selector=..., tab_id=...)               # click an element
#   browser_wait_for(selector=..., tab_id=..., timeout_ms=...)  # wait for render
#   browser_get_markdown(tab_id=...)   -> {markdown, title, url, success}
#   browser_query_all(selector=..., tab_id=...) / browser_screenshot(tab_id=...)

def run(task):
    # 1. open the site (opens a new tab; keep its id)
    nav = browser_navigate(url="https://example.com/search")
    tab = nav["tab_id"]

    # 2. type the task into the input box
    browser_fill(selector="#q", value=task, tab_id=tab)

    # 3. click the submit button
    browser_click(selector="button[type=submit]", tab_id=tab)

    # 4. wait for the page to render the result
    browser_wait_for(selector="#results", tab_id=tab, timeout_ms=15000)

    # 5. read the rendered content and return the markdown text
    result = browser_get_markdown(tab_id=tab)
    return result["markdown"]
