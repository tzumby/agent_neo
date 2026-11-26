// The model providers' keys (agentd 0.20.0): pasted here, kept on the desktop
// for agentd alone, and used from the next model call, with no deploy.

import { busyButton, h } from "./dom.js";

/** What people call a provider's key. */
export function keyLabel(provider) {
  return provider?.id === "together" ? "Together AI API key" : `${provider?.name || "model"} API key`;
}

/** Where a provider's key comes from, in words. */
export function keySource(provider) {
  if (provider?.key === "app") return "Saved in the app";
  if (provider?.key === "deployment") return "From the deployment";
  return "No key yet";
}

/** The provider (of `status.model_providers`) whose model this is, if any. */
export function providerOf(status, model) {
  return (status?.model_providers || []).find((p) => p.models?.includes(model) || p.helper === model) || null;
}

/**
 * A secure field for a provider's key and its Save button. Not a <form>: it
 * also sits in the composer, which is one. `onSaved` gets the desktop's answer
 * once it took the key (`checked` false: the provider couldn't be asked).
 */
export function modelKeyForm(store, provider, { onSaved, onCancel } = {}) {
  const input = h("input", {
    class: "field key-value", type: "password", autocomplete: "off", spellcheck: "false",
    placeholder: provider.id === "anthropic" ? "sk-ant-…" : "Paste the key", "aria-label": keyLabel(provider),
  });
  const save = h("button", { class: "button button-primary", type: "button" }, "Save key");
  const cancel = onCancel ? h("button", { class: "button button-quiet", type: "button" }, "Cancel") : null;
  const problem = h("p", { class: "error", role: "alert", hidden: true });
  const submit = () => {
    if (!input.value.trim()) return input.focus();
    busyButton(save, async () => {
      problem.hidden = true;
      try {
        const result = await store.api.saveModelKey(provider.id, input.value);
        input.value = "";
        await store.reloadStatus();
        onSaved?.(result);
      } catch (error) {
        problem.hidden = false;
        problem.textContent = error.message;
        input.focus();
      }
    });
  };
  save.addEventListener("click", submit);
  cancel?.addEventListener("click", () => onCancel());
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      submit();
    }
  });
  return h("div", { class: "model-key-form" },
    h("label", { class: "label" }, keyLabel(provider), input),
    problem,
    h("div", { class: "card-actions" }, cancel, save),
    h("p", { class: "muted" }, `Kept only on your desktop and sent only to ${provider.name}. Your agent's commands never get it.`));
}

/** What to tell the person once a key is saved. */
export function savedWords(provider, result) {
  return result?.checked === false
    ? `Saved your ${keyLabel(provider)}. ${provider.name} couldn't be reached to check it; your next message will tell.`
    : `Saved your ${keyLabel(provider)}. ${provider.name} accepted it.`;
}
