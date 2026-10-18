class DomainError(Exception):
    def __init__(self, status: int, code: str, message: str, **details):
        super().__init__(message)
        self.status = status
        self.payload = {"error": code, "message": message, **details}
