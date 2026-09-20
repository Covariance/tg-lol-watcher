"""Platform -> regional routing. The only place this mapping lives."""

from __future__ import annotations

# platform routing value (summoner, spectator) -> regional routing value (account, match)
PLATFORM_TO_REGION: dict[str, str] = {
    "na1": "americas",
    "br1": "americas",
    "la1": "americas",
    "la2": "americas",
    "euw1": "europe",
    "eun1": "europe",
    "tr1": "europe",
    "ru": "europe",
    "me1": "europe",
    "kr": "asia",
    "jp1": "asia",
    "oc1": "sea",
    "ph2": "sea",
    "sg2": "sea",
    "th2": "sea",
    "tw2": "sea",
    "vn2": "sea",
}

PLATFORMS = frozenset(PLATFORM_TO_REGION)


def is_platform(value: str) -> bool:
    return value.lower() in PLATFORMS


def region_for(platform: str) -> str:
    try:
        return PLATFORM_TO_REGION[platform.lower()]
    except KeyError:
        raise ValueError(f"unknown platform {platform!r}") from None
