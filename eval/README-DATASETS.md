# Eval datasets — manual fetch playbook

This document explains how to fetch the **real upstream datasets** for each
benchmark. Phase 3c ships hand-written 5-task fixtures so the adapters are
testable end-to-end without internet access; this playbook is the path to
running against the real ~300/~1266/~289 task sets.

All real-data fetches are **opt-in** — not run by default. Eval CI runs
against the fixtures; release-time evaluation uses the upstream data.

## Online-Mind2Web (300 tasks, OSU-NLP-Group)

Upstream: <https://github.com/OSU-NLP-Group/Online-Mind2Web>

The upstream repo is a 450MB+ archive of evaluation results (screenshots,
result.json files). The 300 task definitions themselves are derived from
the per-task directories under `data/`. We don't submodule it — too heavy.

To fetch and convert:

```bash
cd /ai/project/nevoflux-agent
mkdir -p eval/benchmarks/online-mind2web-full
git clone --depth 1 https://github.com/OSU-NLP-Group/Online-Mind2Web.git \
    /tmp/om2w-upstream
# Convert per-task result.json files into our fixture format:
# (Conversion script to be added in Phase 3d; for now manually inspect
# /tmp/om2w-upstream/data/example/<task_id>/result.json for the schema.)
```

Once converted, point the adapter at the converted fixture:

```bash
NEVOFLUX_OM2W_FIXTURE=eval/benchmarks/online-mind2web-full.json \
just eval-dev online-mind2web 50
```

The adapter's `OnlineMind2Web::with_fixture(path)` constructor accepts an
explicit path; the env-var override hook lands in Phase 3d when this is
formalised.

## BrowseComp (1266 tasks, OpenAI)

Upstream: <https://github.com/openai/simple-evals> (see `browsecomp_eval.py`)

BrowseComp data lives in an XOR-encrypted CSV at OpenAI's blob storage:
`https://openaipublic.blob.core.windows.net/simple-evals/browse_comp_test_set.csv`

The encryption is **deliberate** — OpenAI doesn't want models trained on
the answers. Each row has a `password` column; the question and correct
answer columns are base64+XOR encrypted with that password.

The decryption logic (Python reference from `browsecomp_eval.py`):

```python
def derive_key(password: str, length: int) -> bytes:
    hasher = hashlib.sha256()
    hasher.update(password.encode())
    key = hasher.digest()
    return key * (length // len(key)) + key[: length % len(key)]

def decrypt(ciphertext_b64: str, password: str) -> str:
    encrypted = base64.b64decode(ciphertext_b64)
    key = derive_key(password, len(encrypted))
    decrypted = bytes(a ^ b for a, b in zip(encrypted, key))
    return decrypted.decode()
```

A Rust port + CSV downloader lands in Phase 3d. Until then, run real
BrowseComp via the OpenAI simple-evals Python harness directly:

```bash
git clone https://github.com/openai/simple-evals /tmp/simple-evals
cd /tmp/simple-evals
pip install -r requirements.txt  # if a requirements.txt exists; otherwise inspect imports
python browsecomp_eval.py --help  # confirm CLI shape, then point at the daemon's HTTP bridge
```

## BrowseComp-ZH (289 tasks, HKUST/Phantom-AI)

Upstream: <https://huggingface.co/datasets/Phantom-AI/BrowseComp-ZH> (parquet)

HuggingFace dataset format. To fetch:

```bash
pip install datasets
python -c "
from datasets import load_dataset
ds = load_dataset('Phantom-AI/BrowseComp-ZH', split='test')
ds.to_json('eval/benchmarks/browsecomp-zh-full.jsonl')
"
```

The resulting `.jsonl` has one task per line. The Phase 3d adapter will
parse it directly; for now, hand-pick interesting examples into the
fixture file.

## Fixture format (current Phase 3c)

All three benchmark fixtures share the same wire shape:

```json
{
  "version": "phase3c-fixture-v1",
  "source": "human-readable provenance string",
  "tasks": [
    { "id": "...", "question": "...", "answer": "..." }
  ]
}
```

Online-Mind2Web's variant adds `url` and `instruction` fields (plus an
`evaluation_criteria` string for WebJudge) — see Phase 3b's
`online-mind2web-fixture.json`.

## When to use real data vs fixtures

| Mode | Use case |
|---|---|
| **Fixtures (default)** | CI smoke, development iteration, daemon-side regression testing |
| **Real upstream data** | Release-time scoring, leaderboard submissions, marketing claims |

Real-data runs require an actual LLM API key (no mock fallback for
benchmark answers) and may incur cost — BrowseComp's full 1266 tasks
times Sonnet 4.6 input + output = ~$20-30 per run.
