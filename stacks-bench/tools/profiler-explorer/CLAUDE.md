# Claude Code Guidelines

See [AGENTS.md](./AGENTS.md) for comprehensive documentation on this codebase.

## Quick Reference

**What is this?** A web-based profiler trace explorer for Stacks Bench benchmark data.

**Stack:** Flask backend + React/Vite frontend + SQLite database

**Key files:**
- [app.py](./app.py) - REST API with recursive CTE queries for trace tree building
- [web/src/App.jsx](./web/src/App.jsx) - Main React component with tree transformations
- [static/app.js](./static/app.js) - Vanilla JS fallback UI

**Run locally:**
```bash
python app.py        # Backend on :8800
cd web && npm run dev  # Frontend on :5173
```

**Database:** `~/.stacks-bench/appdata/stacks-bench.db` (override with `STACKS_BENCH_DB` env var)
