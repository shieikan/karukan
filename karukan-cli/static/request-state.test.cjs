const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const { createRequestCoordinator } = require('./request-state.js');

const STATIC_DIR = __dirname;

class FakeElement {
    constructor(id, classNames = []) {
        this.id = id;
        this.value = '';
        this._textContent = '';
        this._innerHTML = '';
        Object.defineProperty(this, 'textContent', {
            get: () => this._textContent,
            set: (value) => {
                this._textContent = String(value);
            },
        });
        Object.defineProperty(this, 'innerHTML', {
            get: () => this._innerHTML || this._textContent,
            set: (value) => {
                this._innerHTML = String(value);
            },
        });
        this.className = classNames.join(' ');
        this.listeners = new Map();
        this.attributes = new Map();
        this.focused = false;
        this.classList = {
            add: (...names) => {
                const current = new Set(this.className.split(/\s+/).filter(Boolean));
                names.forEach((name) => current.add(name));
                this.className = [...current].join(' ');
            },
            remove: (...names) => {
                const removed = new Set(names);
                this.className = this.className.split(/\s+/).filter((name) => name && !removed.has(name)).join(' ');
            },
            contains: (name) => this.className.split(/\s+/).includes(name),
            toggle: (name, force) => {
                const shouldAdd = force === undefined ? !this.className.split(/\s+/).includes(name) : force;
                if (shouldAdd) this.classList.add(name);
                else this.classList.remove(name);
                return shouldAdd;
            },
        };
    }

    addEventListener(type, listener) {
        const listeners = this.listeners.get(type) || [];
        listeners.push(listener);
        this.listeners.set(type, listeners);
    }

    dispatchEvent(event) {
        const listeners = this.listeners.get(event.type) || [];
        return Promise.all(listeners.map((listener) => listener(event)));
    }

    focus() {
        this.focused = true;
    }

    getAttribute(name) {
        return this.attributes.get(name) ?? null;
    }

    setAttribute(name, value) {
        this.attributes.set(name, String(value));
    }
}

function createFakeDocument() {
    const ids = [
        'romaji-input', 'hiragana-output', 'buffer-display', 'direct-hiragana-input',
        'kanji-candidates', 'inference-time', 'clear-btn', 'token-display', 'token-count',
        'romaji-mode-btn', 'hiragana-mode-btn', 'romaji-section', 'hiragana-section',
        'model-select', 'model-status', 'num-candidates-input', 'context-input',
        'beam-search-type', 'romaji-char-count', 'hiragana-char-count',
        'direct-hiragana-char-count', 'kanji-char-count',
    ];
    const elements = new Map(ids.map((id) => [id, new FakeElement(id)]));
    elements.get('num-candidates-input').value = '1';
    elements.get('beam-search-type').value = 'true';
    elements.get('romaji-section').className = '';
    elements.get('hiragana-section').className = 'hidden';

    const exampleButtons = [
        new FakeElement('example-1', ['example-btn']),
        new FakeElement('example-2', ['example-btn']),
    ];
    exampleButtons[0].setAttribute('data-text', 'konnnichiha');
    exampleButtons[1].setAttribute('data-text', 'arigatou');
    const contextExampleButtons = [new FakeElement('example-context-1', ['example-btn-ctx'])];
    contextExampleButtons[0].setAttribute('data-text', 'はいしゃ');
    contextExampleButtons[0].setAttribute('data-context', '歯が痛いので');
    const listeners = new Map();

    return {
        getElementById(id) {
            return elements.get(id) || null;
        },
        querySelectorAll(selector) {
            if (selector === '.example-btn') return exampleButtons;
            if (selector === '.example-btn-ctx') return contextExampleButtons;
            return [];
        },
        createElement() {
            return new FakeElement('created');
        },
        addEventListener(type, listener) {
            const current = listeners.get(type) || [];
            current.push(listener);
            listeners.set(type, current);
        },
        dispatchEvent(event) {
            const current = listeners.get(event.type) || [];
            return Promise.all(current.map((listener) => listener(event)));
        },
        elements,
        exampleButtons,
        contextExampleButtons,
    };
}

function createControlledFetch() {
    const calls = [];
    function fetch(url, options = {}) {
        let resolve;
        let reject;
        const promise = new Promise((promiseResolve, promiseReject) => {
            resolve = promiseResolve;
            reject = promiseReject;
        });
        calls.push({ url, options, resolve, reject, promise });
        return promise;
    }
    return { fetch, calls };
}

function responseFromJson(value, ok = true) {
    return {
        ok,
        async json() {
            return value;
        },
        async text() {
            return typeof value === 'string' ? value : JSON.stringify(value);
        },
    };
}

function createBrowserHarness() {
    const document = createFakeDocument();
    const controlledFetch = createControlledFetch();
    const elements = {
        romajiInput: document.getElementById('romaji-input'),
        directHiraganaInput: document.getElementById('direct-hiragana-input'),
        hiraganaOutput: document.getElementById('hiragana-output'),
        bufferDisplay: document.getElementById('buffer-display'),
        kanjiCandidates: document.getElementById('kanji-candidates'),
        inferenceTime: document.getElementById('inference-time'),
        tokenDisplay: document.getElementById('token-display'),
        tokenCount: document.getElementById('token-count'),
        clearBtn: document.getElementById('clear-btn'),
        romajiModeBtn: document.getElementById('romaji-mode-btn'),
        hiraganaModeBtn: document.getElementById('hiragana-mode-btn'),
        modelSelect: document.getElementById('model-select'),
        contextInput: document.getElementById('context-input'),
        numCandidatesInput: document.getElementById('num-candidates-input'),
        beamSearchTypeSelect: document.getElementById('beam-search-type'),
        romajiCharCount: document.getElementById('romaji-char-count'),
        hiraganaCharCount: document.getElementById('hiragana-char-count'),
        directHiraganaCharCount: document.getElementById('direct-hiragana-char-count'),
        kanjiCharCount: document.getElementById('kanji-char-count'),
    };
    const timers = [];
    const timerHandles = new Set();
    let nextTimerId = 1;
    const setTimeoutForTest = (callback, delay) => {
        const handle = { id: nextTimerId++, callback, delay, cancelled: false };
        timers.push(handle);
        timerHandles.add(handle);
        return handle;
    };
    const clearTimeoutForTest = (handle) => {
        if (handle) handle.cancelled = true;
        timerHandles.delete(handle);
    };
    const context = vm.createContext({
        AbortController,
        TextDecoder,
        Uint8Array,
        clearTimeout: clearTimeoutForTest,
        console: { log() {}, error() {} },
        document,
        fetch: controlledFetch.fetch,
        navigator: { clipboard: { async writeText() {} } },
        setTimeout: setTimeoutForTest,
    });

    return {
        context,
        document,
        elements,
        calls: controlledFetch.calls,
        timers,
        exampleButtons: document.querySelectorAll('.example-btn'),
        contextExampleButtons: document.querySelectorAll('.example-btn-ctx'),
        loadScripts() {
            const requestState = fs.readFileSync(path.join(STATIC_DIR, 'request-state.js'), 'utf8');
            const app = fs.readFileSync(path.join(STATIC_DIR, 'app.js'), 'utf8');
            vm.runInContext(requestState, context, { filename: 'request-state.js' });
            vm.runInContext(app, context, { filename: 'app.js' });
        },
        dispatchDOMContentLoaded() {
            return document.dispatchEvent({ type: 'DOMContentLoaded' });
        },
        evaluate(source) {
            return vm.runInContext(source, context);
        },
    };
}

function findCall(calls, url, occurrence = 0) {
    const matching = calls.filter((call) => call.url === url);
    assert.ok(matching[occurrence], `expected ${url} call #${occurrence + 1}`);
    return matching[occurrence];
}

async function flushPromises() {
    await new Promise((resolve) => setImmediate(resolve));
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
}

function makeSnapshot() {
    return {
        input: 'konnichiha',
        context: '今日は',
        model: 'model-a',
        mode: 'romaji',
        candidateCount: '1',
        beamSetting: 'true',
    };
}

test('out-of-order old success and old error cannot mutate current output', () => {
    const snapshot = makeSnapshot();
    const state = { candidates: 'current', inferenceTime: 'current', error: '' };
    const coordinator = createRequestCoordinator({ getSnapshot: () => snapshot });
    const oldTicket = coordinator.begin('kanji');

    snapshot.input = 'arigatou';
    coordinator.invalidate();
    const currentTicket = coordinator.begin('kanji');

    assert.equal(coordinator.commit(oldTicket, () => {
        state.candidates = 'old success';
        state.inferenceTime = 'old metrics';
    }), false);
    assert.equal(coordinator.commit(oldTicket, () => {
        state.candidates = 'old error';
        state.error = 'old error';
    }), false);
    assert.deepEqual(state, { candidates: 'current', inferenceTime: 'current', error: '' });

    assert.equal(coordinator.commit(currentTicket, () => {
        state.candidates = 'new success';
        state.inferenceTime = 'new metrics';
    }), true);
    assert.deepEqual(state, { candidates: 'new success', inferenceTime: 'new metrics', error: '' });
});

test('a current request error clears every derived output before reporting the error', () => {
    const snapshot = makeSnapshot();
    const state = {
        candidates: 'old candidates',
        inferenceTime: '12.3 ms',
        tokenDisplay: 'old tokens',
        tokenCount: 'old token count',
        hiragana: 'こんにちは',
        buffer: 'buffer',
        counts: { romaji: '(10chars)', hiragana: '(5chars)', directHiragana: '(5chars)', kanji: '(2chars)' },
        error: '',
    };
    const coordinator = createRequestCoordinator({ getSnapshot: () => snapshot });
    const ticket = coordinator.begin('kanji');

    assert.equal(coordinator.commit(ticket, () => {
        state.candidates = '';
        state.inferenceTime = '';
        state.tokenDisplay = '';
        state.tokenCount = '';
        state.hiragana = '';
        state.buffer = '';
        state.counts = { romaji: '', hiragana: '', directHiragana: '', kanji: '' };
        state.error = 'Kanji conversion unavailable';
    }), true);

    assert.deepEqual(state, {
        candidates: '',
        inferenceTime: '',
        tokenDisplay: '',
        tokenCount: '',
        hiragana: '',
        buffer: '',
        counts: { romaji: '', hiragana: '', directHiragana: '', kanji: '' },
        error: 'Kanji conversion unavailable',
    });
});

test('every named input and setting invalidates older work', () => {
    for (const field of ['input', 'context', 'model', 'mode', 'candidateCount', 'beamSetting']) {
        const snapshot = makeSnapshot();
        const coordinator = createRequestCoordinator({ getSnapshot: () => snapshot });
        const oldTicket = coordinator.begin('kanji');

        snapshot[field] = `${snapshot[field]}-changed`;
        coordinator.invalidate();

        assert.equal(coordinator.isCurrent(oldTicket), false, `${field} should make the ticket stale`);
        assert.equal(oldTicket.signal.aborted, true, `${field} should abort older work`);
    }
});

test('romaji and kanji requests are independently abortable in one generation', () => {
    const snapshot = makeSnapshot();
    const coordinator = createRequestCoordinator({ getSnapshot: () => snapshot });
    const romajiTicket = coordinator.begin('romaji');
    const kanjiTicket = coordinator.begin('kanji');

    const replacementRomajiTicket = coordinator.begin('romaji');
    assert.equal(romajiTicket.signal.aborted, true);
    assert.equal(kanjiTicket.signal.aborted, false);
    assert.equal(coordinator.isCurrent(kanjiTicket), true);
    assert.equal(coordinator.isCurrent(replacementRomajiTicket), true);

    const replacementKanjiTicket = coordinator.begin('kanji');
    assert.equal(kanjiTicket.signal.aborted, true);
    assert.equal(replacementRomajiTicket.signal.aborted, false);
    assert.equal(coordinator.isCurrent(replacementKanjiTicket), true);
});

test('index loads request-state.js before app.js', () => {
    const html = fs.readFileSync(path.join(STATIC_DIR, 'index.html'), 'utf8');
    const scriptSources = [...html.matchAll(/<script\s+src="([^"]+)"/g)].map((match) => match[1]);
    const requestStateIndex = scriptSources.findIndex((source) => source.startsWith('request-state.js'));
    const appIndex = scriptSources.findIndex((source) => source.startsWith('app.js'));

    assert.ok(requestStateIndex >= 0, 'index.html must load request-state.js');
    assert.ok(appIndex >= 0, 'index.html must load app.js');
    assert.ok(requestStateIndex < appIndex, 'request-state.js must load before app.js');
});

async function bootBrowserHarness() {
    const harness = createBrowserHarness();
    assert.doesNotThrow(() => harness.loadScripts());
    const bootPromise = harness.dispatchDOMContentLoaded();
    const modelCall = findCall(harness.calls, '/api/models');
    modelCall.resolve(responseFromJson({
        models: [{ id: 'model-a', name: 'Model A' }],
        default: 'model-a',
    }));
    await bootPromise;
    return harness;
}

test('browser scripts boot without a ReferenceError', async () => {
    const harness = await bootBrowserHarness();
    assert.equal(harness.elements.romajiInput.listeners.has('input'), true);
    assert.equal(harness.elements.directHiraganaInput.listeners.has('input'), true);
});

test('clearAll aborts active requests and blocks late old success and error after new input', async () => {
    const harness = await bootBrowserHarness();
    const { elements } = harness;

    elements.romajiInput.value = 'furui';
    const oldRomajiPromise = harness.evaluate(
        "handleRomajiInput({ target: document.getElementById('romaji-input') })",
    );
    const oldRomajiCall = findCall(harness.calls, '/api/convert');

    const oldKanjiPromise = harness.evaluate("convertToKanji('ふるい')");
    const oldKanjiCall = findCall(harness.calls, '/api/kanji/convert');

    elements.hiraganaOutput.textContent = '古い出力';
    elements.bufferDisplay.textContent = '古いバッファ';
    elements.kanjiCandidates.innerHTML = '<div>古い候補</div>';
    elements.inferenceTime.textContent = '12.3 ms';
    elements.tokenDisplay.innerHTML = '<div>古いトークン</div>';
    elements.tokenCount.textContent = '1 candidates';
    elements.romajiCharCount.textContent = '(5chars)';
    elements.hiraganaCharCount.textContent = '(5chars)';
    elements.directHiraganaCharCount.textContent = '(3chars)';
    elements.kanjiCharCount.textContent = '(2chars)';

    const clearPromise = harness.evaluate('clearAll()');
    const resetCall = findCall(harness.calls, '/api/reset');

    assert.equal(oldRomajiCall.options.signal.aborted, true);
    assert.equal(oldKanjiCall.options.signal.aborted, true);
    assert.equal(elements.romajiInput.value, '');
    assert.equal(elements.directHiraganaInput.value, '');
    assert.equal(elements.hiraganaOutput.textContent, '');
    assert.equal(elements.bufferDisplay.textContent, '');
    assert.equal(elements.kanjiCandidates.innerHTML, '<p class="placeholder-text">Type to see kanji candidates</p>');
    assert.equal(elements.inferenceTime.textContent, '');
    assert.equal(elements.tokenDisplay.innerHTML, '<p class="placeholder-text">Tokens will appear here</p>');
    assert.equal(elements.tokenCount.textContent, '');
    assert.equal(elements.romajiCharCount.textContent, '');
    assert.equal(elements.hiraganaCharCount.textContent, '');
    assert.equal(elements.directHiraganaCharCount.textContent, '');
    assert.equal(elements.kanjiCharCount.textContent, '');

    resetCall.resolve(responseFromJson({}));
    await clearPromise;

    elements.romajiInput.value = 'atarashii';
    const newRomajiPromise = harness.evaluate(
        "handleRomajiInput({ target: document.getElementById('romaji-input') })",
    );
    const newRomajiCall = findCall(harness.calls, '/api/convert', 1);
    newRomajiCall.resolve(responseFromJson({ output: '新しい', buffer: '' }));
    await newRomajiPromise;

    oldRomajiCall.resolve(responseFromJson({ output: '古い出力', buffer: '古い' }));
    oldKanjiCall.reject(new Error('old kanji request failed'));
    await Promise.all([oldRomajiPromise, oldKanjiPromise]);

    assert.equal(elements.hiraganaOutput.textContent, '新しい');
    assert.equal(elements.bufferDisplay.textContent, '');
    assert.equal(elements.kanjiCandidates.innerHTML, '<p class="placeholder-text">Type to see kanji candidates</p>');
    assert.equal(elements.inferenceTime.textContent, '');
    assert.equal(elements.tokenDisplay.innerHTML, '<p class="placeholder-text">Tokens will appear here</p>');
    assert.equal(elements.tokenCount.textContent, '');
});

test('clearAll cancels a pending kanji debounce before reset completes', async () => {
    const harness = await bootBrowserHarness();
    const { elements } = harness;

    elements.directHiraganaInput.value = 'かな';
    await harness.evaluate(
        "handleDirectHiraganaInput({ target: document.getElementById('direct-hiragana-input') })",
    );
    const debounce = harness.timers.at(-1);
    assert.equal(debounce.delay, 300);
    assert.equal(debounce.cancelled, false);

    const clearPromise = harness.evaluate('clearAll()');
    const resetCall = findCall(harness.calls, '/api/reset');
    assert.equal(debounce.cancelled, true);

    resetCall.resolve(responseFromJson({}));
    await clearPromise;
    assert.equal(harness.calls.filter((call) => call.url === '/api/kanji/convert').length, 0);
});

test('delayed model adoption aborts early work and reissues current input with the new model', async () => {
    const harness = createBrowserHarness();
    harness.loadScripts();
    const bootPromise = harness.dispatchDOMContentLoaded();
    const modelsCall = findCall(harness.calls, '/api/models');

    harness.elements.romajiInput.value = 'konnichiha';
    const earlyRomajiPromise = harness.elements.romajiInput.dispatchEvent({
        type: 'input',
        target: harness.elements.romajiInput,
    });
    const earlyRomajiCall = findCall(harness.calls, '/api/convert');

    modelsCall.resolve(responseFromJson({
        models: [{ id: 'model-b', name: 'Model B' }],
        default: 'model-b',
    }));
    await flushPromises();

    const replacementRomajiCall = findCall(harness.calls, '/api/convert', 1);
    assert.equal(earlyRomajiCall.options.signal.aborted, true);

    replacementRomajiCall.resolve(responseFromJson({ output: 'こんにちは', buffer: '' }));
    earlyRomajiCall.resolve(responseFromJson({ output: '古い', buffer: '' }));
    await Promise.all([earlyRomajiPromise, bootPromise]);

    const debounce = harness.timers.at(-1);
    assert.equal(debounce.cancelled, false);
    debounce.callback();
    const kanjiCall = findCall(harness.calls, '/api/kanji/convert');
    assert.equal(JSON.parse(kanjiCall.options.body).model, 'model-b');
    kanjiCall.resolve(responseFromJson({
        candidates: ['こんにちは'],
        inference_time_ms: 1,
        model: 'model-b',
        candidate_tokens: [],
    }));
    await flushPromises();
});

test('only the latest example continuation may write state after overlapping resets', async () => {
    const harness = await bootBrowserHarness();
    const { exampleButtons, elements } = harness;
    elements.romajiInput.focused = false;

    const firstClick = exampleButtons[0].dispatchEvent({ type: 'click', target: exampleButtons[0] });
    const firstReset = findCall(harness.calls, '/api/reset');
    const secondClick = exampleButtons[1].dispatchEvent({ type: 'click', target: exampleButtons[1] });
    const secondReset = findCall(harness.calls, '/api/reset', 1);

    assert.equal(firstReset.options.signal.aborted, true);
    firstReset.resolve(responseFromJson({}));
    await flushPromises();
    assert.equal(harness.calls.filter((call) => call.url === '/api/convert').length, 0);

    secondReset.resolve(responseFromJson({}));
    await flushPromises();
    assert.equal(elements.romajiInput.value, 'arigatou');
    const currentRomajiCall = findCall(harness.calls, '/api/convert');
    currentRomajiCall.resolve(responseFromJson({ output: 'ありがとう', buffer: '' }));

    await Promise.all([firstClick, secondClick]);
    assert.equal(elements.romajiInput.value, 'arigatou');
    assert.equal(elements.contextInput.value, '');
});

test('context example applies context, mode, input, and conversion only after its current reset', async () => {
    const harness = await bootBrowserHarness();
    const example = harness.contextExampleButtons[0];
    const clickPromise = example.dispatchEvent({ type: 'click', target: example });
    const resetCall = findCall(harness.calls, '/api/reset');

    assert.equal(harness.elements.contextInput.value, '');
    assert.equal(harness.elements.directHiraganaInput.value, '');
    resetCall.resolve(responseFromJson({}));
    await flushPromises();

    assert.equal(harness.elements.contextInput.value, '歯が痛いので');
    assert.equal(harness.elements.directHiraganaInput.value, 'はいしゃ');
    assert.equal(harness.elements.hiraganaModeBtn.classList.contains('active'), true);
    const kanjiCall = findCall(harness.calls, '/api/kanji/convert');
    assert.equal(JSON.parse(kanjiCall.options.body).context, '歯が痛いので');
    kanjiCall.resolve(responseFromJson({
        candidates: ['歯医者'],
        inference_time_ms: 1,
        model: 'model-a',
        candidate_tokens: [],
    }));
    await clickPromise;
    assert.match(harness.elements.kanjiCandidates.innerHTML, /歯医者/);
});

test('new typing invalidates a pending clear reset and blocks stale focus continuation', async () => {
    const harness = await bootBrowserHarness();
    const { elements } = harness;
    elements.romajiInput.focused = false;

    const clearPromise = elements.clearBtn.dispatchEvent({ type: 'click', target: elements.clearBtn });
    const resetCall = findCall(harness.calls, '/api/reset');

    elements.romajiInput.value = 'atarashii';
    const inputPromise = elements.romajiInput.dispatchEvent({
        type: 'input',
        target: elements.romajiInput,
    });
    const currentRomajiCall = findCall(harness.calls, '/api/convert');
    assert.equal(resetCall.options.signal.aborted, true);

    resetCall.resolve(responseFromJson({}));
    currentRomajiCall.resolve(responseFromJson({ output: 'あたらしい', buffer: '' }));
    await Promise.all([clearPromise, inputPromise]);

    assert.equal(elements.romajiInput.value, 'atarashii');
    assert.equal(elements.romajiInput.focused, false);
});

test('live control events abort stale requests and ignore late success and error continuations', async () => {
    const harness = await bootBrowserHarness();
    const { elements } = harness;

    elements.romajiInput.value = 'furui';
    const romajiPromise = elements.romajiInput.dispatchEvent({ type: 'input', target: elements.romajiInput });
    const romajiCall = findCall(harness.calls, '/api/convert');

    elements.contextInput.value = '今日は';
    const contextPromise = elements.contextInput.dispatchEvent({ type: 'input', target: elements.contextInput });
    const contextCall = findCall(harness.calls, '/api/convert', 1);

    elements.numCandidatesInput.value = '2';
    const candidatePromise = elements.numCandidatesInput.dispatchEvent({
        type: 'change',
        target: elements.numCandidatesInput,
    });
    const candidateCall = findCall(harness.calls, '/api/convert', 2);

    elements.beamSearchTypeSelect.value = 'd1_greedy';
    const beamPromise = elements.beamSearchTypeSelect.dispatchEvent({
        type: 'change',
        target: elements.beamSearchTypeSelect,
    });
    const beamCall = findCall(harness.calls, '/api/convert', 3);

    elements.modelSelect.value = 'model-b';
    const modelPromise = elements.modelSelect.dispatchEvent({ type: 'change', target: elements.modelSelect });
    const modelCall = findCall(harness.calls, '/api/convert', 4);

    assert.equal(romajiCall.options.signal.aborted, true);
    assert.equal(contextCall.options.signal.aborted, true);
    assert.equal(candidateCall.options.signal.aborted, true);
    assert.equal(beamCall.options.signal.aborted, true);

    await elements.hiraganaModeBtn.dispatchEvent({ type: 'click', target: elements.hiraganaModeBtn });
    const resetCall = findCall(harness.calls, '/api/reset');
    assert.equal(modelCall.options.signal.aborted, true);
    resetCall.resolve(responseFromJson({}));
    await flushPromises();

    elements.contextInput.value = '文脈';
    await elements.contextInput.dispatchEvent({ type: 'input', target: elements.contextInput });
    elements.directHiraganaInput.value = 'かな';
    await elements.directHiraganaInput.dispatchEvent({ type: 'input', target: elements.directHiraganaInput });
    const debounce = harness.timers.at(-1);
    debounce.callback();
    const currentKanjiCall = findCall(harness.calls, '/api/kanji/convert');
    assert.deepEqual(JSON.parse(currentKanjiCall.options.body), {
        hiragana: 'かな',
        context: '文脈',
        num_candidates: 2,
        model: 'model-b',
        beam_search_type: 'd1_greedy',
    });

    elements.hiraganaOutput.textContent = 'CURRENT HIRAGANA';
    romajiCall.resolve(responseFromJson({ output: '古い', buffer: '' }));
    contextCall.reject(new Error('late context error'));
    candidateCall.resolve(responseFromJson({ output: '古い候補', buffer: '' }));
    beamCall.reject(new Error('late beam error'));
    modelCall.resolve(responseFromJson({ output: '古いモデル', buffer: '' }));
    currentKanjiCall.resolve(responseFromJson({
        candidates: ['現在の候補'],
        inference_time_ms: 2,
        model: 'model-b',
        candidate_tokens: [],
    }));

    await Promise.all([romajiPromise, contextPromise, candidatePromise, beamPromise, modelPromise]);
    await flushPromises();
    assert.equal(elements.hiraganaOutput.textContent, 'CURRENT HIRAGANA');
    assert.match(elements.kanjiCandidates.innerHTML, /現在の候補/);
    assert.doesNotMatch(elements.kanjiCandidates.innerHTML, /古い/);
});

test('current AbortError leaves current output intact while current non-abort errors clean derived output', async () => {
    const harness = await bootBrowserHarness();
    const { elements } = harness;

    elements.romajiInput.value = 'konnichiha';
    const abortPromise = elements.romajiInput.dispatchEvent({ type: 'input', target: elements.romajiInput });
    const abortCall = findCall(harness.calls, '/api/convert');
    elements.hiraganaOutput.textContent = 'CURRENT ABORT OUTPUT';
    abortCall.reject({ name: 'AbortError' });
    await abortPromise;
    assert.equal(elements.hiraganaOutput.textContent, 'CURRENT ABORT OUTPUT');

    elements.romajiInput.value = 'arigatou';
    const errorPromise = elements.romajiInput.dispatchEvent({ type: 'input', target: elements.romajiInput });
    const errorCall = findCall(harness.calls, '/api/convert', 1);
    elements.hiraganaOutput.textContent = 'STALE OUTPUT';
    elements.kanjiCandidates.innerHTML = '<div>STALE CANDIDATES</div>';
    errorCall.reject(new Error('current romaji error'));
    await errorPromise;
    assert.equal(elements.hiraganaOutput.textContent, 'Error');
    assert.equal(elements.kanjiCandidates.innerHTML, '<p class="placeholder-text">Type to see kanji candidates</p>');
});

test('current kanji AbortError is ignored while current HTTP errors clear derived output', async () => {
    const harness = await bootBrowserHarness();
    const { elements } = harness;

    const abortPromise = harness.evaluate("convertToKanji('かな')");
    const abortCall = findCall(harness.calls, '/api/kanji/convert');
    elements.kanjiCandidates.innerHTML = '<div>CURRENT CANDIDATES</div>';
    abortCall.reject({ name: 'AbortError' });
    await abortPromise;
    assert.equal(elements.kanjiCandidates.innerHTML, '<div>CURRENT CANDIDATES</div>');

    const errorPromise = harness.evaluate("convertToKanji('かな')");
    const errorCall = findCall(harness.calls, '/api/kanji/convert', 1);
    elements.kanjiCandidates.innerHTML = '<div>STALE CANDIDATES</div>';
    errorCall.resolve(responseFromJson('server error', false));
    await errorPromise;
    assert.equal(elements.kanjiCandidates.innerHTML, '<p class="error-text">Error: server error</p>');
    assert.equal(elements.hiraganaOutput.textContent, '');
});
