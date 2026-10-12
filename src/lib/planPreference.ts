export type PlanPreference = "any" | "pro" | "plus" | "max" | "team" | "enterprise" | "education" | "free";

export const planPreferences: Record<"codex" | "claude", { value: PlanPreference; label: string }[]> = {
  codex: [
    { value: "any", label: "등급 무관 (먼저 확인된 여유 계정)" },
    { value: "pro", label: "Pro 계열 우선 (Prolite 포함)" },
    { value: "plus", label: "Plus 우선" },
    { value: "team", label: "Team / Business 우선" },
    { value: "enterprise", label: "Enterprise 우선" },
    { value: "education", label: "Edu 우선" },
    { value: "free", label: "Free 우선" },
  ],
  claude: [
    { value: "any", label: "등급 무관 (할당량이 가장 여유 있는 계정)" },
    { value: "max", label: "Max 계열 우선" },
    { value: "pro", label: "Pro 우선" },
    { value: "team", label: "Team 우선" },
    { value: "enterprise", label: "Enterprise 우선" },
  ],
};
