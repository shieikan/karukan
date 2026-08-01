(function (root, factory) {
    if (typeof module === 'object' && module.exports) {
        module.exports = factory();
    } else {
        root.KarukanRequestState = factory();
    }
})(typeof globalThis !== 'undefined' ? globalThis : this, function () {
    function cloneSnapshot(value) {
        if (Array.isArray(value)) return value.map(cloneSnapshot);
        if (value && typeof value === 'object') {
            return Object.keys(value).reduce((copy, key) => {
                copy[key] = cloneSnapshot(value[key]);
                return copy;
            }, {});
        }
        return value;
    }

    function snapshotsEqual(left, right) {
        if (Object.is(left, right)) return true;
        if (!left || !right || typeof left !== 'object' || typeof right !== 'object') return false;

        const leftKeys = Object.keys(left);
        const rightKeys = Object.keys(right);
        if (leftKeys.length !== rightKeys.length) return false;

        return leftKeys.every((key) => Object.prototype.hasOwnProperty.call(right, key) &&
            snapshotsEqual(left[key], right[key]));
    }

    function createRequestCoordinator({ getSnapshot }) {
        if (typeof getSnapshot !== 'function') {
            throw new TypeError('getSnapshot must be a function');
        }

        let generation = 0;
        let currentSnapshot = cloneSnapshot(getSnapshot());
        const activeTickets = new Map();

        function readSnapshot() {
            return cloneSnapshot(getSnapshot());
        }

        function begin(kind) {
            if (typeof kind !== 'string' || kind.length === 0) {
                throw new TypeError('request kind must be a non-empty string');
            }

            const previousTicket = activeTickets.get(kind);
            if (previousTicket) previousTicket.controller.abort();

            const controller = new AbortController();
            const ticket = {
                kind,
                generation,
                snapshot: readSnapshot(),
                controller,
                signal: controller.signal,
            };
            activeTickets.set(kind, ticket);
            return ticket;
        }

        function invalidate(nextSnapshot = readSnapshot()) {
            activeTickets.forEach((ticket) => ticket.controller.abort());
            activeTickets.clear();
            generation += 1;
            currentSnapshot = cloneSnapshot(nextSnapshot);
            return generation;
        }

        function isSnapshotCurrent(snapshot) {
            const liveSnapshot = readSnapshot();
            return snapshotsEqual(snapshot, currentSnapshot) && snapshotsEqual(snapshot, liveSnapshot);
        }

        function isCurrent(ticket) {
            return Boolean(ticket) &&
                ticket.generation === generation &&
                !ticket.signal.aborted &&
                isSnapshotCurrent(ticket.snapshot);
        }

        function commit(ticket, mutation) {
            if (!isCurrent(ticket)) return false;
            mutation();
            return true;
        }

        function finish(ticket) {
            if (activeTickets.get(ticket && ticket.kind) === ticket) {
                activeTickets.delete(ticket.kind);
            }
        }

        return {
            begin,
            commit,
            finish,
            invalidate,
            isCurrent,
            isSnapshotCurrent,
            getGeneration: () => generation,
        };
    }

    return { createRequestCoordinator };
});
