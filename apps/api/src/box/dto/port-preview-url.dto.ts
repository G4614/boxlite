/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { ApiProperty, ApiSchema } from '@nestjs/swagger'
import { IsString } from 'class-validator'

@ApiSchema({ name: 'PortPreviewUrl' })
export class PortPreviewUrlDto {
  @ApiProperty({
    description: 'ID of the box',
    example: '123456',
  })
  @IsString()
  boxId: string

  @ApiProperty({
    description: 'Preview url',
    example: 'https://{port}-{encodedBoxId}.{proxyDomain}',
  })
  @IsString()
  url: string

  @ApiProperty({
    description: 'Access token',
    example: 'ul67qtv-jl6wb9z5o3eii-ljqt9qed6l',
  })
  @IsString()
  token: string
}
