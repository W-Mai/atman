<script lang="ts">
  import type { Dtos } from "../src/index.ts";

  let { form, writable, issue }: {
    form: Dtos["FlowForm"];
    writable: boolean;
    issue: (reply: Dtos["FlowInteractionReply"]) => void;
  } = $props();
  let error = $state("");

  function submit(event: SubmitEvent & { currentTarget: HTMLFormElement }) {
    event.preventDefault();
    if (!writable) return;
    const fields = new FormData(event.currentTarget);
    const answers: Dtos["FlowFormAnswer"][] = [];
    let bytes = 0;
    for (const [index, question] of form.questions.entries()) {
      const name = `answer-${index}`;
      const value = fields.get(name);
      switch (question.input.kind) {
        case "confirm":
          if (value !== "yes" && value !== "no") return;
          answers.push({ kind: "confirm", value: value === "yes" });
          break;
        case "single_select":
          if (value === null || value === "") return;
          answers.push({ kind: "select", index: Number(value) });
          break;
        case "multi_select": {
          const indices = fields.getAll(name).map(Number);
          const { min, max } = question.input;
          if (min != null && indices.length < min) { error = `Question ${index + 1}: choose at least ${min}.`; return; }
          if (max != null && indices.length > max) { error = `Question ${index + 1}: choose up to ${max}.`; return; }
          answers.push({ kind: "multi_select", indices });
          break;
        }
        case "text": {
          const text = String(value ?? "");
          bytes += new TextEncoder().encode(text).length;
          if (bytes > 16384) { error = "Shorten the text answers to 16 KB total."; return; }
          answers.push({ kind: "text", text });
          break;
        }
      }
    }
    error = "";
    issue({ kind: "answer_form", answers });
  }
</script>

<form onsubmit={submit} oninput={() => { error = ""; }}>
  <fieldset class="form-fields" disabled={!writable}>
    <legend>{form.timing === "waiting" ? "Waiting for an answer" : "Late answer"}</legend>
    {#if form.timing === "deferred"}<p class="hint">An answer will enter the Flow’s inbox.</p>{/if}
    {#each form.questions as question, index (index)}
      {@const name = `answer-${index}`}
      {@const input = question.input}
      {#if input.kind === "confirm" || input.kind === "multi_select"}
        <fieldset class="choices">
          <legend>{question.prompt}</legend>
          {#if input.kind === "confirm"}
            <label><input type="radio" {name} value="yes" required /> Yes</label>
            <label><input type="radio" {name} value="no" required /> No</label>
          {:else}
            {#each input.options as option, optionIndex (optionIndex)}
              <label><input type="checkbox" {name} value={optionIndex} /> {option}</label>
            {/each}
          {/if}
        </fieldset>
      {:else}
        <label class="field">
          <span>{question.prompt}</span>
          {#if input.kind === "single_select"}
            <select {name} required>
              <option value="" selected disabled>Choose an option</option>
              {#each input.options as option, optionIndex (optionIndex)}
                <option value={optionIndex}>{option}</option>
              {/each}
            </select>
          {:else if input.multiline}
            <textarea {name} rows="3" maxlength="16384" placeholder={input.placeholder ?? undefined}></textarea>
          {:else}
            <input type="text" {name} maxlength="16384" placeholder={input.placeholder ?? undefined} />
          {/if}
        </label>
      {/if}
    {/each}
    {#if error}<p class="error" role="status">{error}</p>{/if}
    <div class="actions">
      <button type="submit" class="primary">{form.timing === "waiting" ? "Answer" : "Send to inbox"}</button>
      <button type="button" onclick={() => issue({ kind: "reject_form" })}>{form.timing === "waiting" ? "Decline" : "Dismiss form"}</button>
    </div>
  </fieldset>
</form>

<style>
  fieldset { min-inline-size: 0; border: 0; margin: 0; padding: 0; }
  .form-fields { display: grid; gap: .65rem; }
  .form-fields > legend { margin-block-end: .45rem; color: var(--accent); }
  .field, .choices { display: grid; gap: .25rem; }
  .choices legend { margin-block-end: .25rem; }
  .choices label { display: flex; gap: .4rem; align-items: baseline; }
  .choices input { flex: none; accent-color: var(--accent); }
  .field input, textarea, select { min-inline-size: 0; max-inline-size: 100%; }
  textarea { resize: vertical; }
  .actions { display: flex; gap: .35rem; flex-wrap: wrap; }
  .actions button { padding: .25rem .55rem; }
  .hint, .error { margin: 0; }
  .hint { color: var(--muted); }
  .error { color: var(--danger); }
</style>
