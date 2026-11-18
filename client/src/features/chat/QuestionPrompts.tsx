import { useState } from "react";
import { MessageCircleQuestion, SendHorizontal } from "lucide-react";
import { Button } from "@/components/ui/button";
import { answerQuestion } from "@/lib/hub";
import { useProject, useSession, type LiveQuestion } from "@/store/session";

/**
 * The agent's ask_user questions (native turns, and Claude Code / Codex turns through the
 * bridge): pick an option, or type an answer of your own.
 */
export function QuestionPrompts() {
  const questions = useSession((s) => s.questions);
  const { readOnly } = useProject();
  if (questions.length === 0) return null;
  return (
    <div className="space-y-3 border-t bg-primary/5 px-4 py-3 sm:px-6" data-testid="questions">
      {questions.map((q) => (
        <Question key={q.id} q={q} readOnly={!!readOnly} />
      ))}
    </div>
  );
}

function Question({ q, readOnly }: { q: LiveQuestion; readOnly: boolean }) {
  const { id: pid } = useProject();
  const [own, setOwn] = useState("");
  const [sending, setSending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const send = async (answer: string) => {
    if (!answer.trim() || sending) return;
    setSending(true);
    setError(null);
    try {
      if (!(await answerQuestion(pid, q.id, answer))) setError("That question is no longer waiting.");
    } catch (e) {
      setError(String(e));
    } finally {
      setSending(false);
    }
  };
  return (
    <div className="space-y-2 text-sm" data-testid="question">
      <div className="flex items-start gap-2">
        <MessageCircleQuestion className="mt-0.5 size-4 shrink-0 text-primary" />
        <span className="whitespace-pre-wrap font-medium">{q.question}</span>
      </div>
      {readOnly ? (
        <p className="text-xs text-muted-foreground">Waiting for an answer (this tab is read-only).</p>
      ) : (
        <>
          <div className="flex flex-wrap gap-1.5">
            {q.options.map((o, i) => (
              <Button key={i} size="sm" variant="outline" disabled={sending} onClick={() => void send(o)} data-testid="question-option">
                {o}
              </Button>
            ))}
          </div>
          <form
            className="flex items-center gap-2"
            onSubmit={(e) => {
              e.preventDefault();
              void send(own);
            }}
          >
            <input
              value={own}
              onChange={(e) => setOwn(e.target.value)}
              placeholder="Or type your own answer…"
              className="h-8 min-w-0 flex-1 rounded-md border bg-background px-2 text-sm outline-none focus:ring-1 focus:ring-ring"
              data-testid="question-own"
            />
            <Button size="sm" type="submit" disabled={!own.trim() || sending} title="Send this answer">
              <SendHorizontal />
            </Button>
          </form>
          {error && <p className="text-xs text-destructive">{error}</p>}
        </>
      )}
    </div>
  );
}
