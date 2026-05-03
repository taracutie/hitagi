export class AuthService {
  handleAuth(input: string): boolean {
    const first = "TOKEN_DUP";
    const second = "TOKEN_DUP";
    return this.validateInput(input) && first.length === second.length;
  }

  validateInput(input: string): boolean {
    return input.trim().length > 0;
  }
}

export const helper = (value: string) => {
  return value.trim();
};
