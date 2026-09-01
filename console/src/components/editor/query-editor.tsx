"use client";
import * as React from "react";
import { EditorState, StateEffect, StateField, RangeSetBuilder } from "@codemirror/state";
import { EditorView, keymap, highlightActiveLine, lineNumbers, placeholder as cmPlaceholder, Decoration, type DecorationSet } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import { autocompletion, closeBrackets, closeBracketsKeymap, completionKeymap, type CompletionSource } from "@codemirror/autocomplete";
import { bracketMatching, indentOnInput } from "@codemirror/language";
import { sqlSupport, pqlSupport } from "./languages";
import { cn } from "@/lib/cn";

/** The span the planner refused, pushed into the editor so it underlines in place. */
const setRefusal = StateEffect.define<[number, number] | null>();

const refusalMark = Decoration.mark({ class: "cm-refused-span" });

const refusalField = StateField.define<DecorationSet>({
  create: () => Decoration.none,
  update(deco, tr) {
    for (const e of tr.effects) {
      if (e.is(setRefusal)) {
        if (!e.value) return Decoration.none;
        const [from, to] = e.value;
        const max = tr.state.doc.length;
        if (from >= max) return Decoration.none;
        const b = new RangeSetBuilder<Decoration>();
        b.add(from, Math.min(to, max), refusalMark);
        return b.finish();
      }
    }
    // Any edit clears it: the refusal described the text as it was.
    return tr.docChanged ? Decoration.none : deco;
  },
  provide: (f) => EditorView.decorations.from(f),
});

const theme = EditorView.theme({
  "&": { fontSize: "13px", height: "100%", backgroundColor: "transparent", color: "hsl(var(--fg))" },
  "&.cm-focused": { outline: "none" },
  ".cm-scroller": { fontFamily: "var(--font-mono)", lineHeight: "1.65", overflow: "auto" },
  ".cm-content": { padding: "10px 0", caretColor: "hsl(var(--accent))" },
  ".cm-gutters": { backgroundColor: "transparent", border: "none", color: "hsl(var(--fg-faint))", paddingRight: "6px" },
  ".cm-lineNumbers .cm-gutterElement": { minWidth: "26px", fontSize: "11px" },
  ".cm-activeLine": { backgroundColor: "hsl(var(--surface-raised) / 0.55)" },
  ".cm-activeLineGutter": { backgroundColor: "transparent", color: "hsl(var(--fg-muted))" },
  ".cm-selectionBackground, ::selection": { backgroundColor: "hsl(var(--accent) / 0.24) !important" },
  ".cm-cursor": { borderLeftColor: "hsl(var(--accent))", borderLeftWidth: "2px" },
  ".cm-matchingBracket": { backgroundColor: "hsl(var(--accent) / 0.2)", outline: "none" },
  ".cm-refused-span": {
    backgroundColor: "hsl(var(--refused) / 0.2)",
    textDecoration: "underline wavy hsl(var(--refused))",
    textUnderlineOffset: "4px",
    borderRadius: "2px",
  },
  ".cm-tooltip": {
    backgroundColor: "hsl(var(--overlay))", border: "1px solid hsl(var(--border))",
    borderRadius: "var(--radius)", boxShadow: "var(--elev-3)", fontFamily: "var(--font-mono)", fontSize: "12px",
  },
  ".cm-tooltip-autocomplete > ul > li": { padding: "3px 8px", color: "hsl(var(--fg))" },
  ".cm-tooltip-autocomplete > ul > li[aria-selected]": { backgroundColor: "hsl(var(--accent) / 0.18)", color: "hsl(var(--fg))" },
  ".cm-completionDetail": { color: "hsl(var(--fg-faint))", fontStyle: "normal", marginLeft: "10px", fontSize: "11px" },
  ".cm-completionInfo": {
    backgroundColor: "hsl(var(--overlay))", border: "1px solid hsl(var(--border))",
    borderRadius: "var(--radius)", padding: "6px 8px", maxWidth: "300px",
    fontFamily: "var(--font-sans)", lineHeight: "1.5",
  },
  ".cm-placeholder": { color: "hsl(var(--fg-faint))" },
});

export interface QueryEditorProps {
  value: string;
  onChange: (v: string) => void;
  language: "sql" | "pql";
  completion?: CompletionSource;
  /** Byte span the planner refused, underlined in place until the text changes. */
  refusedSpan?: [number, number] | null;
  onRun?: () => void;
  placeholder?: string;
  className?: string;
  readOnly?: boolean;
}

export function QueryEditor({
  value, onChange, language, completion, refusedSpan, onRun, placeholder, className, readOnly,
}: QueryEditorProps) {
  const host = React.useRef<HTMLDivElement>(null);
  const view = React.useRef<EditorView | null>(null);
  const onRunRef = React.useRef(onRun);
  const onChangeRef = React.useRef(onChange);
  onRunRef.current = onRun;
  onChangeRef.current = onChange;

  React.useEffect(() => {
    if (!host.current) return;
    const state = EditorState.create({
      doc: value,
      extensions: [
        lineNumbers(),
        history(),
        bracketMatching(),
        closeBrackets(),
        indentOnInput(),
        highlightActiveLine(),
        cmPlaceholder(placeholder ?? ""),
        autocompletion({ override: completion ? [completion] : undefined, activateOnTyping: true, icons: false }),
        keymap.of([
          { key: "Mod-Enter", run: () => { onRunRef.current?.(); return true; }, preventDefault: true },
          { key: "Shift-Enter", run: () => { onRunRef.current?.(); return true; }, preventDefault: true },
          ...closeBracketsKeymap, ...completionKeymap, ...historyKeymap, ...defaultKeymap, indentWithTab,
        ]),
        language === "sql" ? sqlSupport() : pqlSupport(),
        refusalField,
        theme,
        EditorView.lineWrapping,
        EditorState.readOnly.of(!!readOnly),
        EditorView.updateListener.of((u) => { if (u.docChanged) onChangeRef.current(u.state.doc.toString()); }),
      ],
    });
    const v = new EditorView({ state, parent: host.current });
    view.current = v;
    return () => { v.destroy(); view.current = null; };
    // Rebuilt when the grammar or the completion source changes — both are structural.
  }, [language, completion, placeholder, readOnly]);

  // Keep the document in sync when the value is replaced from outside (a rewrite,
  // a history entry, a permalink) without fighting the user's own typing.
  React.useEffect(() => {
    const v = view.current;
    if (!v) return;
    const current = v.state.doc.toString();
    if (current === value) return;
    v.dispatch({ changes: { from: 0, to: current.length, insert: value } });
  }, [value]);

  React.useEffect(() => {
    view.current?.dispatch({ effects: setRefusal.of(refusedSpan ?? null) });
  }, [refusedSpan]);

  return <div ref={host} className={cn("h-full min-h-0 overflow-hidden", className)} />;
}
