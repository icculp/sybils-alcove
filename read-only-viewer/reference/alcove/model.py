"""The shared vocabulary both harnesses map into.

Kept deliberately small: helpers that encode a fact about the data rather than a
preference about presentation. Anything harness-specific belongs in sources/.
"""

from __future__ import annotations

import re
from typing import Any


def new_usage() -> dict[str, int]:
    return {"input": 0, "output": 0, "cache_read": 0, "cache_write": 0, "reasoning": 0}


def add_anthropic_usage(total: dict[str, int], usage: Any) -> None:
    if not isinstance(usage, dict):
        return
    total["input"] += int(usage.get("input_tokens") or 0)
    total["output"] += int(usage.get("output_tokens") or 0)
    total["cache_read"] += int(usage.get("cache_read_input_tokens") or 0)
    total["cache_write"] += int(usage.get("cache_creation_input_tokens") or 0)


def is_real_model(value: Any) -> bool:
    """`<synthetic>` marks harness-injected messages, not a served model.

    Counting it manufactures phantom switch pairs.
    """
    return bool(value) and not str(value).startswith("<")


# A `/model` switch is recorded as a user event carrying the slash command and
# its resolved result. This is the ONLY on-disk record of a switch that never
# served a turn — `message.model` cannot show one, because no assistant event
# exists to carry it. The harness does emit a "model has been changed" reminder,
# but reminders are injected per request and never written to the transcript.
MODEL_SET_RE = re.compile(r"Set model to ([^<\n]+)")
MODEL_ARGS_RE = re.compile(r"<command-args>([^<]*)</command-args>")
# Two stdout shapes exist: the model id ("claude-opus-5[1m]") and a bolded
# display name with a trailing clause ("<ansi>Opus 4.8 (1M context)<ansi> and
# saved as your default for new sessions"). Strip the terminal styling and the
# clause, or the model name comes out as a sentence.
ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
MODEL_TAIL_RE = re.compile(r"\s+and saved as .*$")


def clean_model_name(raw: str) -> str:
    return MODEL_TAIL_RE.sub("", ANSI_RE.sub("", raw)).strip()


def event_text(event: dict[str, Any]) -> str:
    """Flatten an event's content to text; blocks may be a string or a list."""
    content = (event.get("message") or {}).get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return " ".join(b.get("text", "") for b in content if isinstance(b, dict))
    return ""


def push_model(timeline: list[dict[str, str]], model: str, at: str) -> None:
    if not timeline or timeline[-1]["model"] != model:
        timeline.append({"model": model, "at": at})


def push_selection(out: list[dict[str, str]], model: str, at: str,
                   asked: str) -> None:
    if not out or out[-1]["model"] != model:
        out.append({"model": model, "at": at, "requested": asked})


def live_first(item: dict[str, Any]) -> tuple[bool, float]:
    """Running first, then freshest. Used for the session list AND the subagent
    drilldown so the eye moves the same way at both levels; a subagent with no
    transcript has no age and sorts last.
    """
    age = item.get("age_s")
    return (not item.get("live"), age if age is not None else 1e18)
