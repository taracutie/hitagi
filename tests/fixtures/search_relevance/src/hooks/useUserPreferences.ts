import type { UserPreferences } from "../types/user";

export function useUserPreferences(userId: string): UserPreferences {
  return {
    userId,
    theme: "system",
    compactMode: false,
  };
}
