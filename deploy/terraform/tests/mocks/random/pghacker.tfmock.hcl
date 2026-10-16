# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# A fixed result keeps test runs repeatable. It's longer than the 32-character
# cookie secret: the mock doesn't honor length, and the tests check the
# configured length instead.
mock_resource "random_password" {
  defaults = { result = "MockPassword0123456789abcdefghijklmnopq" }
}
