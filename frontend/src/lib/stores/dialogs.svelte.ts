/**
 * Promise-based confirm/prompt dialogs, rendered by <DialogHost> in the root
 * layout. Replaces the browser's native `confirm()`/`prompt()` with in-app
 * modals that match the rest of the UI. One dialog at a time (queued).
 *
 * Dialogs can carry an async `action`: when present the host runs it on submit
 * and only closes the dialog if it resolves. A rejection keeps the dialog open
 * and surfaces an inline error, so failed renames/deletes don't silently vanish.
 */
import { errorMessage } from '$lib/utils/errors';

export interface ConfirmOptions {
	title: string;
	message?: string;
	confirmText?: string;
	cancelText?: string;
	danger?: boolean;
	/** Optional async action run on confirm; rejection keeps the dialog open. */
	action?: () => Promise<void> | void;
}

export interface PromptOptions {
	title: string;
	message?: string;
	defaultValue?: string;
	placeholder?: string;
	confirmText?: string;
	cancelText?: string;
	/**
	 * Pre-select the input text on open. `'name'` selects the filename portion
	 * (excluding the extension) — used by rename so typing replaces just the
	 * stem. `true` selects everything; omit/`false` to leave the caret at end.
	 */
	selectOnOpen?: boolean | 'name';
	/** Optional async action run with the entered value; rejection keeps it open. */
	action?: (value: string) => Promise<void> | void;
}

export interface FormField {
	key: string;
	label: string;
	value?: string;
	placeholder?: string;
	multiline?: boolean;
}

export interface FormOptions {
	title: string;
	message?: string;
	fields: FormField[];
	confirmText?: string;
	cancelText?: string;
	/** Optional async action run with the collected values; rejection keeps it open. */
	action?: (values: Record<string, string>) => Promise<void> | void;
}

type Pending =
	| { kind: 'confirm'; opts: ConfirmOptions; resolve: (v: boolean) => void }
	| { kind: 'prompt'; opts: PromptOptions; resolve: (v: string | null) => void }
	| {
			kind: 'form';
			opts: FormOptions;
			resolve: (v: Record<string, string> | null) => void;
	  };

class DialogStore {
	current = $state<Pending | null>(null);
	/** Inline error message for the current dialog (from a failed action). */
	error = $state<string | null>(null);
	/** True while the current dialog's async action is running. */
	busy = $state(false);
	#queue: Pending[] = [];

	#enqueue(p: Pending) {
		if (this.current) this.#queue.push(p);
		else {
			this.current = p;
			this.error = null;
			this.busy = false;
		}
	}

	#next() {
		this.error = null;
		this.busy = false;
		this.current = this.#queue.shift() ?? null;
	}

	confirm(opts: ConfirmOptions): Promise<boolean> {
		return new Promise((resolve) => this.#enqueue({ kind: 'confirm', opts, resolve }));
	}

	prompt(opts: PromptOptions): Promise<string | null> {
		return new Promise((resolve) => this.#enqueue({ kind: 'prompt', opts, resolve }));
	}

	form(opts: FormOptions): Promise<Record<string, string> | null> {
		return new Promise((resolve) => this.#enqueue({ kind: 'form', opts, resolve }));
	}

	/**
	 * Called by the host when the user confirms (with a value for prompts).
	 * When the dialog carries an `action`, runs it first: on success the dialog
	 * closes and the promise resolves; on failure the dialog stays open with an
	 * inline error and the promise does NOT resolve yet.
	 */
	async resolve(value: boolean | string | Record<string, string> | null) {
		const c = this.current;
		if (!c) return;
		const run = async (action: () => Promise<void> | void): Promise<boolean> => {
			this.busy = true;
			this.error = null;
			try {
				await action();
				return true;
			} catch (err) {
				this.busy = false;
				this.error = errorMessage(err);
				return false; // keep the dialog open
			}
		};
		if (c.kind === 'confirm') {
			if (c.opts.action && !(await run(c.opts.action))) return;
			c.resolve(value as boolean);
		} else if (c.kind === 'prompt') {
			const action = c.opts.action;
			if (action && !(await run(() => action(value as string)))) return;
			c.resolve(value as string | null);
		} else {
			const action = c.opts.action;
			if (action && !(await run(() => action(value as Record<string, string>)))) return;
			c.resolve(value as Record<string, string> | null);
		}
		this.#next();
	}

	/** Cancel/dismiss the current dialog. */
	cancel() {
		const c = this.current;
		if (!c || this.busy) return;
		if (c.kind === 'confirm') c.resolve(false);
		else c.resolve(null);
		this.#next();
	}
}

export const dialogs = new DialogStore();

/** Convenience wrappers. */
export const confirmDialog = (opts: ConfirmOptions) => dialogs.confirm(opts);
export const promptDialog = (opts: PromptOptions) => dialogs.prompt(opts);
export const formDialog = (opts: FormOptions) => dialogs.form(opts);
