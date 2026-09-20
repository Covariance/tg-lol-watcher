from __future__ import annotations

from conftest import MIN, T0, make_match, make_player

from lolwatch.db import SCHEMA_VERSION, Database, key_fingerprint


async def test_schema_is_created_and_versioned(db: Database):
    assert await db.kv_get("schema_version") == str(SCHEMA_VERSION)
    rows = await db._fetchall("SELECT name FROM sqlite_master WHERE type='table'")
    assert {"players", "matches", "sessions", "session_matches", "subscribers", "kv",
     "allowed_users"} <= \
        {r["name"] for r in rows}


async def test_reopening_an_existing_database_is_a_noop_migration(tmp_path):
    path = str(tmp_path / "x.db")
    first = await Database.open(path)
    await first.add_player(make_player())
    await first.close()
    second = await Database.open(path)
    assert [p.riot_id for p in await second.list_players()] == ["Alice#EUW"]
    await second.close()


async def test_match_insert_is_idempotent(db: Database):
    await db.add_player(make_player())
    m = make_match("EUW1_1", T0)
    assert await db.insert_match(m) is True
    assert await db.insert_match(m) is False
    assert await db.known_match_ids("puuid-alice", ["EUW1_1", "EUW1_2"]) == {"EUW1_1"}
    assert len(await db.matches_between("puuid-alice", T0, T0 + 1)) == 1


async def test_matches_between_is_half_open_and_ordered(db: Database):
    for i, name in enumerate(["c", "a", "b"]):
        await db.insert_match(make_match(name, T0 + [30, 0, 10][i] * MIN))
    got = await db.matches_between("puuid-alice", T0, T0 + 30 * MIN)
    assert [m.match_id for m in got] == ["a", "b"]          # end is exclusive, oldest first


async def test_last_seen_only_moves_forward(db: Database):
    await db.add_player(make_player())
    await db.set_last_seen("puuid-alice", T0 + 10)
    await db.set_last_seen("puuid-alice", T0)
    assert (await db.get_player("puuid-alice")).last_seen_ms == T0 + 10


async def test_soft_delete_keeps_history_and_re_adding_reactivates(db: Database):
    await db.add_player(make_player(last_seen_ms=T0))
    await db.insert_match(make_match("m", T0))
    await db.deactivate_player("puuid-alice")
    assert await db.list_players() == []
    assert await db.find_player("alice", "euw") is None
    assert (await db.find_player("alice", "euw", active_only=False)).active is False
    assert len(await db.matches_between("puuid-alice", 0, 2**60)) == 1

    await db.add_player(make_player(name="AliceRenamed"))          # /add again
    p = await db.get_player("puuid-alice")
    assert p.active and p.game_name == "AliceRenamed"
    # Back from /remove: games played while untracked must not be announced.
    assert p.last_seen_ms is None


async def test_find_player_is_case_insensitive(db: Database):
    await db.add_player(make_player())
    assert (await db.find_player("ALICE", "euw")).puuid == "puuid-alice"


async def test_rewrite_puuid_cascades_everywhere(db: Database):
    await db.add_player(make_player())
    await db.insert_match(make_match("m1", T0))
    sid = await db.create_session("puuid-alice", T0, T0 + 30 * MIN, T0 + 45 * MIN)
    await db.attach_match(sid, "m1", "puuid-alice")

    await db.rewrite_puuid("puuid-alice", "puuid-NEW")

    assert await db.get_player("puuid-alice") is None
    assert (await db.get_player("puuid-NEW")).riot_id == "Alice#EUW"
    assert len(await db.matches_between("puuid-NEW", 0, 2**60)) == 1
    assert (await db.get_session(sid)).puuid == "puuid-NEW"
    assert [m.match_id for m in await db.session_matches(sid)] == ["m1"]   # join still works


async def test_session_lifecycle(db: Database):
    await db.add_player(make_player())
    sid = await db.create_session("puuid-alice", T0, T0 + 30 * MIN, T0 + 45 * MIN)
    assert [s.id for s in await db.open_sessions("puuid-alice")] == [sid]
    await db.set_postpone(sid, 2, T0 + 50 * MIN)
    s = await db.get_session(sid)
    assert (s.postpones, s.flush_due_ms) == (2, T0 + 50 * MIN)
    await db.mark_flushed(sid, T0 + 60 * MIN)
    s = await db.get_session(sid)
    assert (s.status, s.notified_at, s.flush_due_ms) == ("flushed", T0 + 60 * MIN, None)
    assert await db.open_sessions() == []


async def test_subscribers_and_kv(db: Database):
    assert await db.add_subscriber(-100, T0) is True
    assert await db.add_subscriber(-100, T0) is False
    assert await db.is_subscriber(-100) and not await db.is_subscriber(5)
    assert await db.subscribers() == [-100]
    assert await db.remove_subscriber(-100) is True
    assert await db.remove_subscriber(-100) is False

    await db.kv_set("k", "1")
    await db.kv_set("k", "2")
    assert await db.kv_get("k") == "2" and await db.kv_get("missing") is None


async def test_failed_transaction_rolls_back(db: Database):
    try:
        async with db.tx() as conn:
            await conn.execute("INSERT INTO kv(k, v) VALUES('half', 'done')")
            raise RuntimeError("boom")
    except RuntimeError:
        pass
    assert await db.kv_get("half") is None


def test_key_fingerprint_is_stable_and_does_not_leak_the_key():
    assert key_fingerprint("RGAPI-secret") == key_fingerprint("RGAPI-secret")
    assert key_fingerprint("a") != key_fingerprint("b")
    assert "secret" not in key_fingerprint("RGAPI-secret")


async def test_re_adding_an_active_player_keeps_the_polling_marker(db: Database):
    await db.add_player(make_player(last_seen_ms=T0))
    await db.add_player(make_player(name="AliceRenamed"))
    assert (await db.get_player("puuid-alice")).last_seen_ms == T0


async def test_find_player_folds_case_beyond_ascii(db: Database):
    await db.add_player(make_player("p-ru", "Павел", "РУС"))
    assert (await db.find_player("павел", "рус")).puuid == "p-ru"


async def test_delete_match_unstores_it(db: Database):
    await db.insert_match(make_match("m1", T0))
    await db.delete_match("m1", "puuid-alice")
    assert await db.known_match_ids("puuid-alice", ["m1"]) == set()


async def test_allowed_users(db: Database):
    assert await db.add_allowed_user(7, 1, T0) is True
    assert await db.add_allowed_user(7, 1, T0) is False
    assert await db.add_allowed_user(3, None, T0) is True
    assert await db.is_allowed_user(7) and not await db.is_allowed_user(8)
    assert await db.allowed_users() == [3, 7]
    assert await db.remove_allowed_user(7) is True
    assert await db.remove_allowed_user(7) is False
    assert not await db.is_allowed_user(7)


async def test_a_version_1_database_upgrades_and_keeps_its_data(tmp_path):
    import aiosqlite

    from lolwatch.db import _SCHEMA_V1

    path = str(tmp_path / "old.db")
    async with aiosqlite.connect(path) as conn:
        await conn.execute("CREATE TABLE kv (k TEXT PRIMARY KEY, v TEXT NOT NULL)")
        for stmt in filter(None, (x.strip() for x in _SCHEMA_V1.split(";"))):
            await conn.execute(stmt)
        await conn.execute("INSERT INTO kv VALUES('schema_version', '1')")
        await conn.execute("INSERT INTO subscribers VALUES(-5, 0)")
        await conn.commit()

    db = await Database.open(path)
    try:
        assert await db.kv_get("schema_version") == str(SCHEMA_VERSION)
        assert await db.subscribers() == [-5]
        assert await db.allowed_users() == []
    finally:
        await db.close()
