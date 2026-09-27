"""Turn the plain-text Wikipedia extracts in _src/ into static pages.

_src/<lang>__<Title>.txt  ->  <lang>/<Title>.html
`== Heading ==` lines become <h2>/<h3>; everything else becomes <p>.
Run from anywhere: python eval/harness/sites/wiki/build.py
"""

import html
import pathlib
import re

HERE = pathlib.Path(__file__).resolve().parent
HEADING = re.compile(r"^(=+)\s*(.*?)\s*=+$")


def render(title: str, lang: str, text: str) -> str:
    body = []
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        m = HEADING.match(line)
        if m:
            level = min(len(m.group(1)), 4)
            body.append(f"<h{level}>{html.escape(m.group(2))}</h{level}>")
        else:
            body.append(f"<p>{html.escape(line)}</p>")
    display = title.replace("_", " ")
    return (f'<!doctype html>\n<html lang="{"zh-CN" if lang == "zh" else "en"}">\n'
            f'<meta charset="utf-8">\n<title>{html.escape(display)} — Wiki mirror</title>\n'
            f"<h1>{html.escape(display)}</h1>\n" + "\n".join(body) + "\n"
            "<footer><p>Text from Wikipedia, CC BY-SA 4.0. See ../ATTRIBUTION.md.</p></footer>\n</html>\n")


def main():
    for src in sorted((HERE / "_src").glob("*.txt")):
        lang, _, title = src.stem.partition("__")
        out = HERE / lang / f"{title}.html"
        out.parent.mkdir(exist_ok=True)
        out.write_bytes(render(title, lang, src.read_text(encoding="utf-8")).encode("utf-8"))
        print(out.relative_to(HERE))


if __name__ == "__main__":
    main()
