---
name: python
description: Python projects and scripts with uv: Python versions, virtual environments, dependencies, one-off tools.
---
# Python with uv

uv is installed. Don't use the system python3 for projects, and never
`sudo pip` or `pip install --user`.

- New project: `uv init report && cd report && uv add requests pandas`, then
  `uv run python main.py`.
- Existing project: with `pyproject.toml`, `uv sync` then `uv run …`; with
  `requirements.txt`, `uv venv && uv pip install -r requirements.txt`, then
  `uv run …`.
- A script with its own dependencies: `uv run --with pandas --with openpyxl script.py`
- One-off tools: `uvx ruff check .`, `uvx --from httpie http GET https://…`
- A specific Python: `uv python install 3.13`, or `uv run --python 3.12 …`.

uv downloads Python builds from GitHub and packages from PyPI; both are
allowed through the proxy.
