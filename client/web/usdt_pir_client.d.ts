/* tslint:disable */
/* eslint-disable */

export class Query {
    private constructor();
    free(): void;
    [Symbol.dispose](): void;
    bytes: Uint8Array;
    id: number;
}

/**
 * What the client must fetch next: `"up-to-date"`, `"full"`, or `"tail"`.
 */
export class SyncPlan {
    private constructor();
    free(): void;
    [Symbol.dispose](): void;
    action: string;
    /**
     * Offset to request the tail from; `0` unless `action == "tail"`.
     */
    from: number;
}

export class UsdtPirClient {
    free(): void;
    [Symbol.dispose](): void;
    applyTail(tail: Uint8Array): void;
    cancel(id: number): boolean;
    /**
     * Decrypt a response into a JSON report.
     */
    decode(id: number, response: Uint8Array): string;
    /**
     * Bootstrap from the server's full directory blob.
     */
    constructor(directory: Uint8Array);
    /**
     * Build an encrypted query. Returns the id to pass back to `decode`.
     */
    query(address: string): Query;
    resync(directory: Uint8Array): void;
    /**
     * The database slot an address resolves to. Diagnostics only.
     */
    slot(address: string): number;
    syncNeed(server_version: bigint, server_tail_len: number): SyncPlan;
    readonly pendingCount: number;
    readonly tailLen: number;
    readonly version: bigint;
}

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_get_query_bytes: (a: number) => [number, number];
    readonly __wbg_get_query_id: (a: number) => number;
    readonly __wbg_get_syncplan_action: (a: number) => [number, number];
    readonly __wbg_query_free: (a: number, b: number) => void;
    readonly __wbg_set_query_bytes: (a: number, b: number, c: number) => void;
    readonly __wbg_set_query_id: (a: number, b: number) => void;
    readonly __wbg_syncplan_free: (a: number, b: number) => void;
    readonly __wbg_usdtpirclient_free: (a: number, b: number) => void;
    readonly usdtpirclient_applyTail: (a: number, b: number, c: number) => [number, number];
    readonly usdtpirclient_cancel: (a: number, b: number) => number;
    readonly usdtpirclient_decode: (a: number, b: number, c: number, d: number) => [number, number, number, number];
    readonly usdtpirclient_new: (a: number, b: number) => [number, number, number];
    readonly usdtpirclient_pendingCount: (a: number) => number;
    readonly usdtpirclient_query: (a: number, b: number, c: number) => [number, number, number];
    readonly usdtpirclient_resync: (a: number, b: number, c: number) => [number, number];
    readonly usdtpirclient_slot: (a: number, b: number, c: number) => [number, number, number];
    readonly usdtpirclient_syncNeed: (a: number, b: bigint, c: number) => number;
    readonly usdtpirclient_tailLen: (a: number) => number;
    readonly usdtpirclient_version: (a: number) => bigint;
    readonly __wbg_get_syncplan_from: (a: number) => number;
    readonly __wbg_set_syncplan_from: (a: number, b: number) => void;
    readonly __wbg_set_syncplan_action: (a: number, b: number, c: number) => void;
    readonly __wbindgen_exn_store: (a: number) => void;
    readonly __externref_table_alloc: () => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __externref_table_dealloc: (a: number) => void;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
