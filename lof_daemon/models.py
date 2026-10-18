from typing import Annotated, Literal

from pydantic import BaseModel, ConfigDict, Field

Identifier = Annotated[str, Field(pattern=r"^[A-Za-z0-9_-]{1,128}$")]


class Model(BaseModel):
    model_config = ConfigDict(extra="forbid", strict=True)


class SmartHistory(Model):
    enabled: bool = False
    keep_recent_minutes: int = Field(default=60, ge=1, le=525600)
    bucket_minutes: int = Field(default=10, ge=1, le=525600)


class HistoryPolicy(Model):
    min_minutes: int = Field(default=10, ge=1, le=10080)
    min_changed_chars: int = Field(default=200, ge=1, le=1000000)
    retention_days: int | None = Field(default=None, ge=1, le=36500)
    expire_protected: bool = False
    smart: SmartHistory = Field(default_factory=SmartHistory)


class LibraryCreate(Model):
    id: Identifier
    name: str = Field(min_length=1, max_length=200)
    history: HistoryPolicy = Field(default_factory=HistoryPolicy)


class PolicyUpdate(Model):
    expected_version: int = Field(ge=1)
    history: HistoryPolicy


class Mutation(Model):
    device_id: Identifier
    mutation_id: Identifier
    expected_version: int = Field(ge=0)


class DocumentMutation(Mutation):
    kind: Literal["episode", "memo", "plot", "project"] = "episode"
    title: str = Field(max_length=1000)
    body: str = Field(max_length=1000000)
    metadata: dict = Field(default_factory=dict)
    deleted: bool = False


class Capture(Model):
    expected_version: int = Field(ge=1)
    device_id: Identifier


class Restore(Mutation):
    expected_version: int = Field(ge=1)


class Resolve(Mutation):
    expected_version: int = Field(ge=1)
    choice: Literal["current", "incoming"]


Password = Annotated[str, Field(min_length=8, max_length=256)]
ServerUrl = Annotated[str, Field(max_length=500, pattern=r"^https?://[^\s/?#]+(/[^\s?#]*)?$")]


class Setup(Model):
    code: str | None = Field(default=None, max_length=32)
    password: Password


class Login(Model):
    password: str = Field(min_length=1, max_length=256)


class PasswordChange(Model):
    current_password: str | None = Field(default=None, max_length=256)
    new_password: Password


class DeviceUpdate(Model):
    name: str = Field(min_length=1, max_length=100)


class PairingCreate(Model):
    name: str = Field(default="", max_length=100)
    role: Literal["device", "admin"] = "device"
    server_url: ServerUrl | None = None


class Pair(Model):
    code: str = Field(min_length=4, max_length=32)
    name: str = Field(default="", max_length=100)
    platform: str = Field(default="", max_length=50)
