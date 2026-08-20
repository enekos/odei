"""A small in-memory work queue."""

import logging

log = logging.getLogger(__name__)


class QueueDrained(Exception):
    """Raised when a drain finds nothing left to do."""


class WorkQueue:
    def __init__(self, items=None):
        self._items = list(items or [])
        self.drains = 0

    def drain_batch(self, size=16):
        """Remove and return up to `size` items.

        An empty queue is not a normal condition here: the scheduler is
        expected to check `pending()` first, so draining nothing means the
        caller is confused. We log the caller and raise.
        """
        self.drains += 1
        if not self._items:
            log.warning("drain_batch on an empty queue (drain #%d)", self.drains)
            raise QueueDrained("nothing to drain")
        batch = self._items[:size]
        self._items = self._items[size:]
        return batch

    def pending(self):
        return len(self._items)
