from __future__ import annotations

import argparse
import os
import sqlite3
from pathlib import Path
from typing import Any, Dict, Iterable, Optional

from flask import Flask, jsonify, request, send_from_directory

DEFAULT_DB_RELATIVE = ".stacks-bench/appdata/stacks-bench.db"

app = Flask(__name__, static_folder="dist", static_url_path="")


def resolve_db_path(db_arg: Optional[str]) -> Path:
    if db_arg:
        return Path(db_arg).expanduser().resolve()
    env_db = os.getenv("STACKS_BENCH_DB")
    if env_db:
        return Path(env_db).expanduser().resolve()
    return Path.cwd().joinpath(DEFAULT_DB_RELATIVE).resolve()


def open_db(db_path: Path) -> sqlite3.Connection:
    conn = sqlite3.connect(str(db_path))
    conn.row_factory = sqlite3.Row
    return conn


def query_all(conn: sqlite3.Connection, sql: str, params: Dict[str, Any]) -> Iterable[Dict[str, Any]]:
    cur = conn.execute(sql, params)
    rows = cur.fetchall()
    return [dict(row) for row in rows]


def parse_optional_int(value: Optional[str], name: str) -> Optional[int]:
    if value is None or value == "":
        return None
    try:
        return int(value)
    except ValueError as exc:
        raise ValueError(f"Invalid {name}: {value}") from exc


@app.route("/")
def index() -> Any:
    return send_from_directory(app.static_folder, "index.html")


@app.route("/<path:path>")
def static_proxy(path: str) -> Any:
    file_path = Path(app.static_folder).joinpath(path)
    if file_path.exists():
        return send_from_directory(app.static_folder, path)
    return send_from_directory(app.static_folder, "index.html")


@app.route("/api/health")
def health() -> Any:
    return jsonify({"ok": True})


@app.route("/api/runs")
def runs() -> Any:
    db_path = app.config["DB_PATH"]
    conn = open_db(db_path)
    try:
        rows = query_all(
            conn,
            """
            SELECT id, run_name, start_time, end_time
            FROM benchmark_run
            ORDER BY id DESC
            LIMIT 200
            """,
            {},
        )
        return jsonify(rows)
    finally:
        conn.close()


@app.route("/api/blocks")
def blocks() -> Any:
    db_path = app.config["DB_PATH"]
    run_id = request.args.get("run_id")
    try:
        run_id_int = parse_optional_int(run_id, "run_id")
    except ValueError as exc:
        return jsonify({"error": str(exc)}), 400

    if run_id_int is None:
        return jsonify([])

    conn = open_db(db_path)
    try:
        rows = query_all(
            conn,
            """
            SELECT DISTINCT sb.id AS stacks_block_id,
                   sb.block_hash_hex,
                   sb.height
            FROM profiler_record pr
            JOIN synthetic_block synth ON synth.id = pr.synthetic_block_id
            JOIN stacks_block sb ON sb.id = synth.stacks_block_id
            WHERE pr.benchmark_run_id = :run_id
            ORDER BY sb.height DESC
            LIMIT 500
            """,
            {"run_id": run_id_int},
        )
        return jsonify(rows)
    finally:
        conn.close()


@app.route("/api/txs")
def txs() -> Any:
    db_path = app.config["DB_PATH"]
    run_id = request.args.get("run_id")
    q = request.args.get("q")
    limit = request.args.get("limit")

    try:
        run_id_int = parse_optional_int(run_id, "run_id")
        limit_int = parse_optional_int(limit, "limit") or 200
    except ValueError as exc:
        return jsonify({"error": str(exc)}), 400

    if run_id_int is None:
        return jsonify([])

    params: Dict[str, Any] = {"run_id": run_id_int, "limit": limit_int}
    filter_sql = ""
    if q:
        params["q"] = q.lower() + "%"
        filter_sql = "AND tx.tx_hash_hex LIKE :q"

    conn = open_db(db_path)
    try:
        rows = query_all(
            conn,
            f"""
            SELECT tx.id AS stacks_tx_id,
                   tx.tx_hash_hex,
                   tx.stacks_block_id
            FROM stacks_tx tx
            JOIN profiler_record pr ON pr.stacks_tx_id = tx.id
            WHERE pr.benchmark_run_id = :run_id
              {filter_sql}
            GROUP BY tx.id
            ORDER BY tx.id DESC
            LIMIT :limit
            """,
            params,
        )
        return jsonify(rows)
    finally:
        conn.close()


@app.route("/api/tx-lookup")
def tx_lookup() -> Any:
    db_path = app.config["DB_PATH"]
    run_id = request.args.get("run_id")
    tx_hash = request.args.get("tx_hash")

    try:
        run_id_int = parse_optional_int(run_id, "run_id")
    except ValueError as exc:
        return jsonify({"error": str(exc)}), 400

    if run_id_int is None or not tx_hash:
        return jsonify({"error": "run_id and tx_hash are required"}), 400

    conn = open_db(db_path)
    try:
        rows = query_all(
            conn,
            """
            SELECT tx.id AS stacks_tx_id,
                   tx.tx_hash_hex
            FROM stacks_tx tx
            JOIN profiler_record pr ON pr.stacks_tx_id = tx.id
            WHERE pr.benchmark_run_id = :run_id
              AND tx.tx_hash_hex = :tx_hash
            LIMIT 1
            """,
            {"run_id": run_id_int, "tx_hash": tx_hash.lower()},
        )
        if not rows:
            return jsonify({"stacks_tx_id": None})
        return jsonify(rows[0])
    finally:
        conn.close()


def trace_sql_tx_mode() -> str:
    return """
    WITH RECURSIVE
    bench AS (
      SELECT id AS benchmark_run_id
      FROM benchmark_run
      WHERE id = :run_id
    ),
    tx_seed AS (
      SELECT pr.benchmark_run_id, pr.id
      FROM profiler_record pr
      JOIN bench b ON b.benchmark_run_id = pr.benchmark_run_id
      WHERE (:stacks_tx_id IS NULL OR pr.stacks_tx_id = :stacks_tx_id)
    ),
    ancestors AS (
      SELECT benchmark_run_id, id
      FROM tx_seed
      UNION ALL
      SELECT p.benchmark_run_id, p.parent_id
      FROM profiler_record p
      JOIN ancestors a
        ON p.benchmark_run_id = a.benchmark_run_id
       AND p.id = a.id
      WHERE p.parent_id IS NOT NULL
    ),
    descendants AS (
      SELECT benchmark_run_id, id
      FROM tx_seed
      UNION ALL
      SELECT c.benchmark_run_id, c.id
      FROM profiler_record c
      JOIN descendants p
        ON c.benchmark_run_id = p.benchmark_run_id
       AND c.parent_id = p.id
    ),
    kept AS (
      SELECT benchmark_run_id, id FROM ancestors
      UNION
      SELECT benchmark_run_id, id FROM descendants
    ),
    payload AS (
      SELECT pr.*, synth.stacks_block_id
      FROM kept k
      JOIN profiler_record pr
        ON pr.benchmark_run_id = k.benchmark_run_id
       AND pr.id = k.id
      JOIN synthetic_block synth
        ON synth.id = pr.synthetic_block_id
    ),
    kv_counts AS (
      SELECT k.id AS id,
             COUNT(prkv.profiler_record_id) AS kv_pairs,
             COALESCE(SUM(prkv.count), 0) AS kv_total
      FROM kept k
      LEFT JOIN profiler_record_kv prkv
        ON prkv.profiler_record_id = k.id
      GROUP BY k.id
    ),
    trace_tree AS (
      SELECT p.*, printf('%09d', p.id) AS sort_path
      FROM payload p
      LEFT JOIN kept pk
        ON pk.benchmark_run_id = p.benchmark_run_id
       AND pk.id = p.parent_id
      WHERE pk.id IS NULL
      UNION ALL
      SELECT c.*, t.sort_path || '.' || printf('%09d', c.child_index) AS sort_path
      FROM payload c
      JOIN trace_tree t
        ON c.benchmark_run_id = t.benchmark_run_id
       AND c.parent_id = t.id
    )
    SELECT
      t.id,
      t.parent_id,
      t.child_index,
      t.depth,
      t.profiler_span_id,
      t.profiler_tag_id,
      t.synthetic_block_id,
      t.stacks_tx_id,
      t.benchmark_run_id,

      t.call_count,
      t.sample_count,
      t.wall_time_us,
      t.self_wall_time_us,
      t.cpu_time_us,
      t.self_cpu_time_us,
      t.expand_factor,
      t.est_wall_us,
      t.est_self_wall_us,
      t.est_cpu_us,
      t.est_self_cpu_us,

      s.name AS span_name,
      s.context AS span_context,
      pt.tag AS tag,
      tx.tx_hash_hex,
      b.block_hash_hex,
      p.address AS contract_issuer,
      c.name AS contract,
      c_fn.name AS contract_fn,

      COALESCE(kvc.kv_pairs, 0) AS kv_pairs,
      COALESCE(kvc.kv_total, 0) AS kv_total,

      prcc.runtime AS clarity_runtime,
      prcc.read_count AS clarity_read_count,
      prcc.read_length AS clarity_read_length,
      prcc.write_count AS clarity_write_count,
      prcc.write_length AS clarity_write_length,

      t.sort_path
    FROM trace_tree t
    JOIN profiler_span s ON s.id = t.profiler_span_id
    LEFT JOIN profiler_tag pt ON pt.id = t.profiler_tag_id
    LEFT JOIN stacks_block b ON b.id = t.stacks_block_id
    LEFT JOIN stacks_tx tx ON tx.id = t.stacks_tx_id
    LEFT JOIN contract c ON c.id = tx.contract_id
    LEFT JOIN contract_fn c_fn ON c_fn.id = tx.contract_fn_id
    LEFT JOIN principal p ON p.id = c.issuer_principal_id
    LEFT JOIN kv_counts kvc ON kvc.id = t.id
    LEFT JOIN profiler_record_clarity_costs prcc ON prcc.profiler_record_id = t.id
    ORDER BY t.sort_path ASC
    """


def trace_sql_run_mode() -> str:
    return """
    WITH RECURSIVE
    bench AS (
      SELECT id AS benchmark_run_id
      FROM benchmark_run
      WHERE id = :run_id
    ),
    run_scope AS (
      SELECT pr.*, synth.stacks_block_id
      FROM profiler_record pr
      JOIN bench b ON b.benchmark_run_id = pr.benchmark_run_id
      JOIN synthetic_block synth ON synth.id = pr.synthetic_block_id
      WHERE (:stacks_tx_id IS NULL OR pr.stacks_tx_id = :stacks_tx_id)
        AND (:stacks_block_id IS NULL OR synth.stacks_block_id = :stacks_block_id)
        AND (:min_wall_us IS NULL OR COALESCE(pr.est_wall_us, pr.wall_time_us) >= :min_wall_us)
    ),
    trace_tree AS (
      SELECT base.*, printf('%09d', base.id) AS sort_path
      FROM run_scope base
      WHERE base.parent_id IS NULL
        AND (:segment_root_id IS NULL OR base.id = :segment_root_id)
      UNION ALL
      SELECT child.*, parent.sort_path || '.' || printf('%09d', child.child_index) AS sort_path
      FROM run_scope child
      JOIN trace_tree parent
        ON child.parent_id = parent.id
       AND child.benchmark_run_id = parent.benchmark_run_id
    ),
    kv_counts AS (
      SELECT profiler_record_id,
             COUNT(*) AS kv_pairs,
             COALESCE(SUM(count), 0) AS kv_total
      FROM profiler_record_kv
      GROUP BY profiler_record_id
    )
    SELECT
      t.id,
      t.parent_id,
      t.child_index,
      t.depth,
      t.profiler_span_id,
      t.profiler_tag_id,
      t.synthetic_block_id,
      t.stacks_tx_id,
      t.benchmark_run_id,

      t.call_count,
      t.sample_count,
      t.wall_time_us,
      t.self_wall_time_us,
      t.cpu_time_us,
      t.self_cpu_time_us,
      t.expand_factor,
      t.est_wall_us,
      t.est_self_wall_us,
      t.est_cpu_us,
      t.est_self_cpu_us,

      s.name AS span_name,
      s.context AS span_context,
      pt.tag AS tag,
      tx.tx_hash_hex,
      b.block_hash_hex,
      p.address AS contract_issuer,
      c.name AS contract,
      c_fn.name AS contract_fn,

      COALESCE(kvc.kv_pairs, 0) AS kv_pairs,
      COALESCE(kvc.kv_total, 0) AS kv_total,

      prcc.runtime AS clarity_runtime,
      prcc.read_count AS clarity_read_count,
      prcc.read_length AS clarity_read_length,
      prcc.write_count AS clarity_write_count,
      prcc.write_length AS clarity_write_length,

      t.sort_path
    FROM trace_tree t
    JOIN profiler_span s ON s.id = t.profiler_span_id
    LEFT JOIN profiler_tag pt ON pt.id = t.profiler_tag_id
    LEFT JOIN stacks_block b ON b.id = t.stacks_block_id
    LEFT JOIN stacks_tx tx ON tx.id = t.stacks_tx_id
    LEFT JOIN contract c ON c.id = tx.contract_id
    LEFT JOIN contract_fn c_fn ON c_fn.id = tx.contract_fn_id
    LEFT JOIN principal p ON p.id = c.issuer_principal_id
    LEFT JOIN kv_counts kvc ON kvc.profiler_record_id = t.id
    LEFT JOIN profiler_record_clarity_costs prcc ON prcc.profiler_record_id = t.id
    ORDER BY t.sort_path ASC
    LIMIT :limit
    """


@app.route("/api/trace")
def trace() -> Any:
    db_path = app.config["DB_PATH"]
    mode = request.args.get("mode", "tx")

    try:
        run_id = parse_optional_int(request.args.get("run_id"), "run_id")
        stacks_tx_id = parse_optional_int(request.args.get("stacks_tx_id"), "stacks_tx_id")
        stacks_block_id = parse_optional_int(request.args.get("stacks_block_id"), "stacks_block_id")
        segment_root_id = parse_optional_int(request.args.get("segment_root_id"), "segment_root_id")
        min_wall_ms = parse_optional_int(request.args.get("min_wall_ms"), "min_wall_ms")
        limit = parse_optional_int(request.args.get("limit"), "limit") or 5000
    except ValueError as exc:
        return jsonify({"error": str(exc)}), 400

    if run_id is None:
        return jsonify({"error": "run_id is required"}), 400

    params = {
        "run_id": run_id,
        "stacks_tx_id": stacks_tx_id,
        "stacks_block_id": stacks_block_id,
        "segment_root_id": segment_root_id,
        "min_wall_us": None if min_wall_ms is None else min_wall_ms * 1000,
        "limit": limit,
    }

    sql = trace_sql_tx_mode() if mode == "tx" else trace_sql_run_mode()

    conn = open_db(db_path)
    try:
        rows = query_all(conn, sql, params)
        return jsonify(rows)
    finally:
        conn.close()


def main() -> None:
    parser = argparse.ArgumentParser(description="stacks-bench profiler explorer")
    parser.add_argument("--db", dest="db", help="Path to stacks-bench.db")
    parser.add_argument("--port", dest="port", type=int, default=8800)
    parser.add_argument("--host", dest="host", default="127.0.0.1")
    args = parser.parse_args()

    db_path = resolve_db_path(args.db)
    if not db_path.exists():
        raise SystemExit(f"Database not found: {db_path}")

    app.config["DB_PATH"] = db_path
    app.run(host=args.host, port=args.port, debug=False)


if __name__ == "__main__":
    main()
