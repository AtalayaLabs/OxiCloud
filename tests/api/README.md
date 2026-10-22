# API functional tests

Tests are written using [Hurl](https://hurl.dev) — a plain-text, CLI-first HTTP testing tool.

## Prerequisites

- [Hurl](https://hurl.dev/docs/installation.html) ≥ 4.0
  Install: `cargo install hurl` or via your package manager

## Configuration

Edit `test.env` to match your local instance. This file is the single source of
truth: `run.sh` sources it for shell variables and passes it to Hurl as
`--variables-file`.

```
base_url=http://localhost:8087
username=admin
email=admin@example.com
password=TestPassword1!
```

## Running the tests

```bash
# First-time setup (run once on a fresh instance)
hurl --variables-file tests/api/test.env --test tests/api/setup.hurl

# Contacts CRUD scenario
hurl --variables-file tests/api/test.env --test tests/api/contacts.hurl

# All scenarios at once
hurl --variables-file tests/api/test.env --test tests/api/setup.hurl tests/api/contacts.hurl

# With full request/response output
hurl --variables-file tests/api/test.env --test --verbose tests/api/contacts.hurl

# Generate an HTML report
hurl --variables-file tests/api/test.env --test --report-html /tmp/hurl-report tests/api/contacts.hurl
```

## Test files

| File | Description |
|---|---|
| `setup.hurl` | One-time admin account creation; also asserts the endpoint is locked afterwards |
| `files-folders.hurl` | Files & folders CRUD scenario (22 steps, see below) |
| `favorites.hurl` | Favorites add/list/remove scenario (11 steps); depends on `files-folders.hurl` state |
| `trash.hurl` | Trash move/restore/purge scenario (16 steps); depends on `files-folders.hurl` state |
| `recent.hurl` | Recent items record/list/clear scenario (6 steps); depends on `files-folders.hurl` state |
| `contacts.hurl` | Full contacts CRUD scenario (14 steps, see below) |
| `test.env` | Variables: `base_url`, `username`, `email`, `password` — used by both Hurl and `run.sh` |

## Scenario: `files-folders.hurl`

| Step | Description |
|---|---|
| 1 | Login – capture JWT token |
| 2 | List root folders – assert exactly 1 (home folder), capture `home_folder_id`, assert `parent_id` is null |
| 3 | Browse home folder sub-folders – assert empty |
| 4 | Browse home folder files – assert empty |
| 5 | Create folder named `"/"` – assert HTTP 400, `error_type == "Invalid Input"` |
| 6 | Create `test1` inside home folder – capture `test1_id`, assert `parent_id` == `home_folder_id` |
| 7 | Create `test2` inside `test1` – capture `test2_id`, assert `parent_id` == `test1_id` |
| 8 | Browse home folder – assert exactly 1 sub-folder (`test1`) |
| 9 | Browse `test1` – assert exactly 1 sub-folder (`test2`), assert `parent_id` |
| 10 | Upload `fixtures/hello.txt` into `test2` – capture `file_id`, assert name/size/mime_type/folder_id |
| 11 | List files in `test2` – assert count=1, `mime_type == text/plain`, `category == Text`, and that `icon_class` / `icon_special_class` / `size_formatted` are absent |
| 12 | Move `test2` from `test1` into home folder – assert `parent_id` == `home_folder_id` |
| 13 | Browse `test1` – assert empty (no more children) |
| 14 | Browse home folder – assert 2 sub-folders (`test1` and `test2`) |
| 15 | Rename `test2` → `test2-renamed` – assert new name, `parent_id` unchanged |
| 16 | Rename `hello.txt` → `hello-renamed.txt` – assert new name, `folder_id` unchanged |
| 17 | Rename `hello-renamed.txt` to `"."` – assert HTTP 400, `error_type == "Invalid Input"` |
| 18 | Upload `fixtures/oxicloud-logo.jpg` into home folder – capture `logo_id`, assert `mime_type == image/jpeg` |
| 19 | List files in home folder – assert count=1, `category == Image` |
| 20 | `GET /api/files/{logo_id}/thumbnail/icon` → HTTP 200 |
| 21 | `GET /api/files/{logo_id}/thumbnail/preview` → HTTP 200 |
| 22 | `GET /api/files/{logo_id}/thumbnail/large` → HTTP 200 |

## Scenario: `favorites.hurl`

Depends on `files-folders.hurl` having run first (test1, test2-renamed/hello-renamed.txt must exist).

| Step | Description |
|---|---|
| 1 | Login – capture JWT token |
| 2 | Assert no favorites yet |
| 3 | Discover item IDs: home folder → contents (test1=$[0], test2-renamed=$[1]) → files in test2-renamed |
| 4 | `POST /api/favorites/file/{file_id}` – add hello-renamed.txt → HTTP 201 |
| 5 | List favorites – assert count=1, item_type=file, item_name=hello-renamed.txt |
| 6 | `POST /api/favorites/folder/{test1_id}` – add test1 → HTTP 201 |
| 7 | List favorites – assert count=2, both IDs present (order-independent) |
| 8 | `DELETE /api/favorites/file/{file_id}` – remove hello-renamed.txt → HTTP 200 |
| 9 | List favorites – assert count=1, item_type=folder, item_name=test1 |
| 10 | Cleanup: `DELETE /api/favorites/folder/{test1_id}` → HTTP 200 |
| 11 | List favorites – assert count=0 |

## Scenario: `trash.hurl`

Depends on `files-folders.hurl` having run first (home folder must exist).

| Step | Description |
|---|---|
| 1 | Login – capture JWT token |
| 2 | Assert trash is empty |
| 3 | Capture home folder ID |
| 4 | Create `to-delete` folder in home folder – capture `to_delete_id` |
| 5 | Upload `hello.txt` into `to-delete` |
| 6 | `DELETE /api/folders/{to_delete_id}` – moves folder to trash → HTTP 204 |
| 7 | List trash – assert count=1, item_type=folder, name=to-delete, original_id matches; capture `trash_id` |
| 8 | List home folder contents – assert `to-delete` is not present |
| 9 | `POST /api/trash/{trash_id}/restore` → HTTP 200, `success == true` |
| 10 | List home folder contents – assert `to-delete` is present |
| 11 | List files in `to-delete` – assert count=1, name=hello.txt |
| 12 | List trash – assert empty (restore removed the entry) |
| 13 | `DELETE /api/folders/{to_delete_id}` – move restored folder to trash again → HTTP 204 |
| 14 | `DELETE /api/trash/empty` – purge trash → HTTP 200, `success == true` |
| 15 | List trash – assert empty after purge |
| 16 | List home folder contents – assert `to-delete` is not present (permanently gone) |

## Scenario: `recent.hurl`

Depends on `files-folders.hurl` having run first (test2-renamed/hello-renamed.txt must exist).
Recent items are not auto-recorded on upload — step 3 explicitly registers the access.

| Step | Description |
|---|---|
| 1 | Login – capture JWT token |
| 2 | Discover `file_id` of hello-renamed.txt via home folder → test2-renamed contents |
| 3 | `POST /api/recent/file/{file_id}` – record access → HTTP 200 |
| 4 | `GET /api/recent` – assert count=1, item_type=file, item_name=hello-renamed.txt |
| 5 | `DELETE /api/recent/clear` – clear all recent items → HTTP 200 |
| 6 | `GET /api/recent` – assert count=0 |

## Scenario: `contacts.hurl`

| Step | Description |
|---|---|
| 1 | Login – capture JWT token |
| 2 | List address books – assert system book is present and read-only |
| 3 | Create personal address book – capture `book_id` |
| 4 | List contacts in new book – assert empty |
| 5 | Create contact John Doe – capture `contact_id` |
| 6 | List contacts – assert exactly 1 result with John Doe's id |
| 7 | Get John Doe – assert all fields, capture `ETag` |
| 8 | Update John Doe (nickname, org, notes) with `If-Match` – assert new values, capture refreshed `ETag` |
| 9 | Delete John Doe with `If-Match` |
| 10 | List contacts – assert empty again |
| 11 | Delete personal address book |
| 12 | List address books – assert `book_id` no longer present |
| 13 | List system address book – assert non-empty collection of OxiCloud users |

## Legacy bash tests

`test.sh` and `common.sh` are the original curl/bash scripts kept for reference.
Run them with `bash tests/api/test.sh` from the repo root (requires `jq`).
