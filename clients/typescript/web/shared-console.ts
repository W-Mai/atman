import { flushSync, mount, unmount } from "svelte";
import type { ApprovalView } from "./approval-activity.ts";
import ConsoleComposer from "./ConsoleComposer.svelte";
import type { ConsoleSnapshot, ConversationConsole } from "./conversation.ts";

const idle: ConsoleSnapshot = {
  text: "", feedback: "", editable: false, canSend: false, canCheck: false,
  canRetry: false, hasPending: false, hasEarlierView: false,
};

type Publication = { snapshot: ConsoleSnapshot; actions: ConversationConsole };

export function sharedConsole(target: HTMLElement, openApprovals: (view: ApprovalView, trigger: HTMLButtonElement) => void) {
  let active: object | undefined;
  let bindingIdentity = 0;
  let current: Publication | undefined;
  const publications = new WeakMap<object, Publication>();
  const node = mount(ConsoleComposer, {
    target,
    props: {
      openApprovals,
      inbox: (identity, action) => { if (identity === bindingIdentity) current?.actions.inbox(action); },
      edit: (value) => current?.actions.edit(value),
      send: () => current?.actions.send(),
      checkDelivery: () => current?.actions.checkDelivery(),
      retry: () => current?.actions.retry(),
      jumpLatest: () => current?.actions.jumpLatest(),
      addImages: (identity, images) => identity === bindingIdentity ? current?.actions.addImages(images) : undefined,
      removeAttachment: (attachment) => current?.actions.removeAttachment(attachment),
      editQuote: (identity, original, text) => identity === bindingIdentity && current
        ? current.actions.editQuote(original, text) : "The conversation changed. Reopen the quote to edit.",
      browseModels: async (query, signal) => current?.actions.browseModels(query, signal),
      editPreference: (identity, revision, value) => {
        if (identity === bindingIdentity) current?.actions.editPreference(revision, value);
      },
      checkPreference: (identity) => { if (identity === bindingIdentity) current?.actions.checkPreference(); },
      retryPreference: (identity) => { if (identity === bindingIdentity) current?.actions.retryPreference(); },
      showHistory: (trigger) => current?.actions.showHistory?.(trigger),
      askQuestion: (trigger) => current?.actions.askQuestion?.(trigger),
    },
  });

  function install(publication: Publication | undefined, force = false): void {
    if (force || current?.actions !== publication?.actions) bindingIdentity++;
    current = publication;
    node.update(publication?.snapshot ?? idle, bindingIdentity);
  }

  return {
    activate(owner?: object): void {
      if (active === owner) return;
      active = owner;
      node.updateApprovals(undefined);
      target.hidden = owner === undefined;
      flushSync(() => install(owner === undefined ? undefined : publications.get(owner), true));
    },
    presentApprovals(view: ApprovalView | undefined): void {
      node.updateApprovals(view);
    },
    publish(owner: object, snapshot: ConsoleSnapshot, actions: ConversationConsole): void {
      const publication = { snapshot, actions };
      publications.set(owner, publication);
      if (active === owner) install(publication);
    },
    destroy(): void {
      current = undefined;
      active = undefined;
      void unmount(node);
    },
  };
}
