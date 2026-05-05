export interface UserProfile {
  id: string;
  displayName: string;
  email: string;
}

export type UserPreferences = {
  userId: string;
  theme: "light" | "dark" | "system";
  compactMode: boolean;
};
