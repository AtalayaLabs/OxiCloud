/**
 * Global Promise-queue store for the Move/Copy conflict resolution modal (`<TransferConflictDialog>`).
 *
 * Mirrors `stores/dialogs.svelte.ts` so every transfer surface (`MoveDialog`, folder-row drop,
 * breadcrumb drop, and sidebar drive drop) awaits one shared confirmation modal without mounting
 * duplicate `<TransferConflictDialog>` instances.
 */
import type {
	TransferItemDecision,
	TransferOperation,
	TransferPreflightResponse
} from '$lib/api/endpoints/transfer';

export interface TransferConflictPrompt {
	operation: TransferOperation;
	targetName?: string;
	totalItems: number;
	preflight: TransferPreflightResponse;
}

export interface PendingTransferConflict extends TransferConflictPrompt {
	resolve: (decisions: TransferItemDecision[] | null) => void;
}

class TransferConflictStore {
	current = $state<PendingTransferConflict | null>(null);
	private queue: PendingTransferConflict[] = [];

	private advance() {
		this.current = this.queue.shift() ?? null;
	}

	prompt(opts: TransferConflictPrompt): Promise<TransferItemDecision[] | null> {
		return new Promise((resolve) => {
			const entry: PendingTransferConflict = { ...opts, resolve };
			if (!this.current) {
				this.current = entry;
			} else {
				this.queue.push(entry);
			}
		});
	}

	confirm(decisions: TransferItemDecision[]) {
		const active = this.current;
		if (!active) return;
		this.advance();
		active.resolve(decisions);
	}

	cancel() {
		const active = this.current;
		if (!active) return;
		this.advance();
		active.resolve(null);
	}
}

export const transferConflictStore = new TransferConflictStore();

export function promptTransferConflicts(
	opts: TransferConflictPrompt
): Promise<TransferItemDecision[] | null> {
	return transferConflictStore.prompt(opts);
}
