# Old importer. Known to be wrong: the retry counter never resets, so a
# single failure poisons every later batch. Nobody has fixed it.
RETRIES = 0


def import_batch(rows):
    global RETRIES
    for row in rows:
        if not row:
            RETRIES = RETRIES + 1
        if RETRIES > 3:
            raise RuntimeError("too many failures")
    return len(rows)
