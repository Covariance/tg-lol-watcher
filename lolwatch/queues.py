"""queue_id -> display group. One table; unknown ids render as `Queue {id}`."""

from __future__ import annotations

_GROUPS: dict[str, tuple[int, ...]] = {
    "Ranked Solo": (420,),
    "Ranked Flex": (440,),
    "Normal": (400, 430, 490),
    "ARAM": (450,),
    "Arena": (1700, 1710),
    "Bots": (830, 840, 850, 870, 880, 890),
    "Clash": (700,),
}

QUEUE_GROUP: dict[int, str] = {qid: name for name, ids in _GROUPS.items() for qid in ids}

# Order groups appear in summaries; anything else sorts after these.
GROUP_ORDER: tuple[str, ...] = tuple(_GROUPS)

# Queues where `win` has no Summoner's Rift meaning (placements, not win/loss).
NO_WIN_LOSS_QUEUES = frozenset({1700, 1710})


def queue_group(queue_id: int) -> str:
    return QUEUE_GROUP.get(queue_id, f"Queue {queue_id}")


def group_sort_key(group: str) -> tuple[int, str]:
    try:
        return (GROUP_ORDER.index(group), group)
    except ValueError:
        return (len(GROUP_ORDER), group)
