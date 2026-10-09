use super::*;

fn header(dimensions: u16, flags: u16, timebase: u32) -> Vec<u8> {
    let mut header = vec![0; 124];
    header[0..2].copy_from_slice(&0xdb99u16.to_be_bytes());
    header[2..4].copy_from_slice(&dimensions.to_be_bytes());
    header[4..6].copy_from_slice(&flags.to_be_bytes());
    header[12..16].copy_from_slice(&timebase.to_be_bytes());
    header
}
fn f64_at(data: &mut [u8], offset: usize, value: f64) {
    data[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
}
fn scalar_key(time: i32, value: f64, incoming: u8, outgoing: u8) -> Vec<u8> {
    let mut row = vec![0; 48];
    row[..4].copy_from_slice(&time.to_be_bytes());
    row[4] = incoming;
    row[5] = outgoing;
    f64_at(&mut row, 8, value);
    f64_at(&mut row, 24, 1.0 / 3.0);
    f64_at(&mut row, 40, 1.0 / 3.0);
    row
}
fn list_header(count: u32, size: u32) -> Vec<u8> {
    let mut data = vec![0; 52];
    data[8..12].copy_from_slice(&count.to_be_bytes());
    data[16..20].copy_from_slice(&size.to_be_bytes());
    data
}
fn animated<'a>(header: &'a [u8], list_header: &'a [u8], data: &'a [u8]) -> Vec<ChunkNode<'a>> {
    vec![
        ChunkNode::Chunk { id: *b"tdb4", data: header },
        ChunkNode::List {
            id: *b"LIST",
            list_type: *b"list",
            children: vec![ChunkNode::Chunk { id: *b"lhd3", data: list_header }, ChunkNode::Chunk { id: *b"ldat", data }],
        },
    ]
}

#[test]
fn scalar_animation_preserves_exact_times_bezier_and_hold() {
    let header = header(1, 0, 24_576);
    let list = list_header(3, 48);
    let data = [scalar_key(-1024, 10.0, 1, 2), scalar_key(24576, 70.0, 2, 3), scalar_key(49152, 90.0, 3, 1)].concat();
    let decoded = decode(&animated(&header, &list, &data), &Value::Scalar(0.0)).unwrap();
    assert_eq!(decoded.keys.len(), 3);
    assert_eq!(decoded.keys[0].time, Tick::from_units(-1, 24));
    assert_eq!(decoded.keys[1].time, Tick::from_units(1, 1));
    assert_eq!(decoded.keys[0].out_interp, Interp::Bezier);
    assert_eq!(decoded.keys[1].out_interp, Interp::Hold);
    assert_eq!(decoded.keys[0].out_ease, vec![Ease::EASY]);
    assert_eq!(effectcraft_keyframe::evaluate(&decoded.keys, Tick::from_units(3, 2), false), Some(Value::Scalar(70.0)));
}

#[test]
fn vector_keys_keep_independent_temporal_eases() {
    let header = header(2, 0, 48_000);
    let list = list_header(1, 88);
    let mut data = vec![0; 88];
    data[4] = 2;
    data[5] = 2;
    for (index, value) in [100.0, 200.0, 3.0, 4.0, 0.25, 0.5, 5.0, 6.0, 0.75, 1.0].into_iter().enumerate() {
        f64_at(&mut data, 8 + index * 8, value);
    }
    let decoded = decode(&animated(&header, &list, &data), &Value::Vec2([0.0; 2])).unwrap();
    assert_eq!(decoded.keys[0].value, Value::Vec2([100.0, 200.0]));
    assert_eq!(decoded.keys[0].in_ease, vec![Ease { speed: 3.0, influence: 0.25 }, Ease { speed: 4.0, influence: 0.5 }]);
    assert_eq!(decoded.keys[0].out_ease, vec![Ease { speed: 5.0, influence: 0.75 }, Ease { speed: 6.0, influence: 1.0 }]);
}

#[test]
fn spatial_keys_preserve_values_and_tangents() {
    let header = header(3, 14, 24_576);
    let list = list_header(1, 128);
    let mut data = vec![0; 128];
    data[4] = 2;
    data[5] = 2;
    f64_at(&mut data, 24, 12.0);
    f64_at(&mut data, 32, 0.25);
    f64_at(&mut data, 40, 18.0);
    f64_at(&mut data, 48, 0.5);
    for (index, value) in [320.0, 240.0, 50.0, -20.0, -30.0, 0.0, 40.0, 60.0, 0.0].into_iter().enumerate() {
        f64_at(&mut data, 56 + index * 8, value);
    }
    let decoded = decode(&animated(&header, &list, &data), &Value::Vec3([0.0; 3])).unwrap();
    assert_eq!(decoded.keys[0].value, Value::Vec3([320.0, 240.0, 50.0]));
    assert_eq!(decoded.keys[0].spatial_in, [-20.0, -30.0, 0.0]);
    assert_eq!(decoded.keys[0].spatial_out, [40.0, 60.0, 0.0]);
    assert!(!decoded.keys[0].spatial_auto);
}

#[test]
fn color_stream_remains_raw_until_caller_converts_channels() {
    let header = header(4, 6, 24_576);
    let list = list_header(1, 152);
    let mut data = vec![0; 152];
    data[4] = 1;
    data[5] = 1;
    for (index, value) in [255.0, 51.0, 102.0, 153.0].into_iter().enumerate() {
        f64_at(&mut data, 56 + index * 8, value);
    }
    let mut decoded = decode(&animated(&header, &list, &data), &Value::Color([0.0; 4])).unwrap();
    assert_eq!(decoded.value, Some(Value::Color([255.0, 51.0, 102.0, 153.0])));
    decoded.map_values(|value| {
        let Value::Color([a, r, g, b]) = value else { panic!() };
        Value::Color([r / 255.0, g / 255.0, b / 255.0, a / 255.0])
    });
    assert_eq!(decoded.keys[0].value, Value::Color([0.2, 0.4, 0.6, 1.0]));
}

#[test]
fn opaque_orientation_is_little_endian_and_expression_is_preserved() {
    let mut header = header(1, 7, 24_576);
    header[8..10].copy_from_slice(&6u16.to_be_bytes());
    let data: Vec<_> = [90.0f64, 300.0, -22.5].into_iter().flat_map(f64::to_le_bytes).collect();
    let nodes = vec![
        ChunkNode::Chunk { id: *b"tdb4", data: &header },
        ChunkNode::Chunk { id: *b"cdat", data: &data },
        ChunkNode::Chunk { id: *b"expr", data: b"time * 5\0" },
    ];
    let decoded = decode(&nodes, &Value::Vec3([0.0; 3])).unwrap();
    assert_eq!(decoded.value, Some(Value::Vec3([90.0, 300.0, -22.5])));
    assert_eq!(decoded.expression.unwrap().text, "time * 5");
}

#[test]
fn malformed_streams_are_errors_not_panics() {
    let header = header(1, 0, 24_576);
    let list = list_header(1, 48);
    let data = scalar_key(0, 50.0, 1, 1);
    for end in 0..data.len() {
        assert!(decode(&animated(&header, &list, &data[..end]), &Value::Scalar(0.0)).is_err());
    }
    let mut bad = header.clone();
    bad[12..16].fill(0);
    assert!(decode(&animated(&bad, &list, &data), &Value::Scalar(0.0)).unwrap_err().contains("zero"));
    let mut bad = data.clone();
    f64_at(&mut bad, 8, f64::NAN);
    assert!(decode(&animated(&header, &list, &bad), &Value::Scalar(0.0)).is_err());
    let list = list_header(100_001, 48);
    assert!(decode(&animated(&header, &list, &data), &Value::Scalar(0.0)).is_err());
    let list = list_header(2, 48);
    let data = [data.clone(), data].concat();
    assert!(decode(&animated(&header, &list, &data), &Value::Scalar(0.0)).unwrap_err().contains("increasing"));
}

#[test]
fn transform_import_converts_percent_units_and_ignores_dormant_axes() {
    let scalar_header = header(1, 1, 24576);
    let vector_header = header(3, 1, 24576);
    let opacity = 0.5f64.to_be_bytes();
    let scale: Vec<_> = [0.75f64, 1.25, 1.0].into_iter().flat_map(f64::to_be_bytes).collect();
    let dormant = 9999.0f64.to_be_bytes();
    let flags = 3u32.to_be_bytes();
    let nodes = vec![
        ChunkNode::Chunk { id: *b"tdmn", data: b"ADBE Transform Group\0" },
        ChunkNode::List {
            id: *b"LIST",
            list_type: *b"tdgp",
            children: vec![
                ChunkNode::Chunk { id: *b"tdmn", data: b"ADBE Opacity\0" },
                ChunkNode::List {
                    id: *b"LIST",
                    list_type: *b"tdbs",
                    children: vec![ChunkNode::Chunk { id: *b"tdb4", data: &scalar_header }, ChunkNode::Chunk { id: *b"cdat", data: &opacity }],
                },
                ChunkNode::Chunk { id: *b"tdmn", data: b"ADBE Scale\0" },
                ChunkNode::List {
                    id: *b"LIST",
                    list_type: *b"tdbs",
                    children: vec![ChunkNode::Chunk { id: *b"tdb4", data: &vector_header }, ChunkNode::Chunk { id: *b"cdat", data: &scale }],
                },
                ChunkNode::Chunk { id: *b"tdmn", data: b"ADBE Position_0\0" },
                ChunkNode::List {
                    id: *b"LIST",
                    list_type: *b"tdbs",
                    children: vec![
                        ChunkNode::Chunk { id: *b"tdsb", data: &flags },
                        ChunkNode::Chunk { id: *b"tdb4", data: &scalar_header },
                        ChunkNode::Chunk { id: *b"cdat", data: &dormant },
                    ],
                },
            ],
        },
    ];
    let decoded = collect_transforms(&nodes).unwrap();
    assert_eq!(decoded.len(), 2);
    assert_eq!(decoded[0].stream.value, Some(Value::Scalar(50.0)));
    assert_eq!(decoded[1].stream.value, Some(Value::Vec3([75.0, 125.0, 100.0])));
}

#[test]
fn opaque_path_key_times_do_not_interpret_pointer_payloads() {
    let header = header(1, 6, 24576);
    let list = list_header(2, 64);
    let mut data = vec![0xff; 128];
    data[..4].copy_from_slice(&0i32.to_be_bytes());
    data[4] = 1;
    data[5] = 2;
    data[64..68].copy_from_slice(&12288i32.to_be_bytes());
    data[68] = 2;
    data[69] = 1;
    let times = key_times(&animated(&header, &list, &data)).unwrap();
    assert_eq!(times, vec![(Tick::ZERO, Interp::Linear, Interp::Bezier), (Tick::from_units(1, 2), Interp::Bezier, Interp::Linear)]);
}

#[test]
#[ignore = "requires EFFECTCRAFT_AEP_TEST_FILE pointing to a local user project"]
fn external_numeric_streams_decode() {
    let bytes = std::fs::read(std::env::var("EFFECTCRAFT_AEP_TEST_FILE").expect("set EFFECTCRAFT_AEP_TEST_FILE")).unwrap();
    let nodes = crate::aep::parse_rifx(&bytes).unwrap();
    fn visit(nodes: &[ChunkNode<'_>], layers: &mut usize, keys: &mut usize, expressions: &mut usize) {
        for node in nodes {
            if let ChunkNode::List { list_type, children, .. } = node {
                if matches!(list_type, b"Layr" | b"AVLa") {
                    *layers += 1;
                    let decoded = collect_transforms(children).unwrap();
                    *keys += decoded.iter().map(|entry| entry.stream.keys.len()).sum::<usize>();
                    *expressions += decoded.iter().filter(|entry| entry.stream.expression.is_some()).count();
                } else {
                    visit(children, layers, keys, expressions);
                }
            }
        }
    }
    let (mut layers, mut keys, mut expressions) = (0, 0, 0);
    visit(&nodes, &mut layers, &mut keys, &mut expressions);
    println!("Decoded {layers} actual layers, {keys} transform/camera keys, {expressions} expressions");
    assert!(layers > 0);
}

#[test]
fn time_remap_keeps_held_source_time_and_skips_disabled_streams() {
    let header = header(1, 0, 24576);
    let list = list_header(1, 48);
    let data = scalar_key(31744, 31.0 / 24.0, 3, 3);
    let active_flags = 1u32.to_be_bytes();
    let mut stream = animated(&header, &list, &data);
    stream.push(ChunkNode::Chunk { id: *b"tdsb", data: &active_flags });
    let nodes =
        vec![ChunkNode::Chunk { id: *b"tdmn", data: b"ADBE Time Remapping\0" }, ChunkNode::List { id: *b"LIST", list_type: *b"tdbs", children: stream }];
    let decoded = collect_transforms(&nodes).unwrap();
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].path, "timeRemap");
    assert_eq!(decoded[0].stream.keys[0].value, Value::Scalar(31.0 / 24.0));
    assert_eq!(decoded[0].stream.keys[0].out_interp, Interp::Hold);
    let disabled_flags = 3u32.to_be_bytes();
    let mut stream = animated(&header, &list, &data);
    stream.push(ChunkNode::Chunk { id: *b"tdsb", data: &disabled_flags });
    let disabled =
        vec![ChunkNode::Chunk { id: *b"tdmn", data: b"ADBE Time Remapping\0" }, ChunkNode::List { id: *b"LIST", list_type: *b"tdbs", children: stream }];
    assert!(collect_transforms(&disabled).unwrap().is_empty());
}

#[test]
fn disabled_expression_state_is_not_reactivated_on_import() {
    let mut header = header(1, 1, 24576);
    header[119] = 1;
    header[120] = 1;
    let value = 15.0f64.to_be_bytes();
    let nodes = vec![
        ChunkNode::Chunk { id: *b"tdb4", data: &header },
        ChunkNode::Chunk { id: *b"cdat", data: &value },
        ChunkNode::Chunk { id: *b"expr", data: b"thisComp.layer(\"Missing\").rotation\0" },
    ];
    let decoded = decode(&nodes, &Value::Scalar(0.0)).unwrap();
    assert!(!decoded.expression.unwrap().enabled);
    assert_eq!(decoded.value, Some(Value::Scalar(15.0)));
    assert_eq!(decoded.warnings.len(), 1);
}

#[test]
fn orientation_conversion_preserves_plane_basis_including_gimbal_lock() {
    use effectcraft_geom::{Mat4, Vec3, vec3};
    for angles in [[90.0, 45.0, 270.0], [90.0, 135.0, 270.0], [270.0, 225.0, 270.0], [0.0, 90.0, 0.0], [0.0, -90.0, 40.0]] {
        let Value::Vec3(converted) = convert_orientation(&Value::Vec3(angles)) else { panic!() };
        let expected = Mat4::rotate_x(angles[0]) * Mat4::rotate_y(angles[1]) * Mat4::rotate_z(angles[2]);
        let actual = Mat4::orientation(Vec3::from(converted));
        for basis in [vec3(1.0, 0.0, 0.0), vec3(0.0, 1.0, 0.0), vec3(0.0, 0.0, 1.0)] {
            let a = actual.apply_vec(basis);
            let b = expected.apply_vec(basis);
            assert!((a.x - b.x).abs() < 1e-8 && (a.y - b.y).abs() < 1e-8 && (a.z - b.z).abs() < 1e-8, "{angles:?}");
        }
    }
}

#[test]
fn omitted_camera_position_uses_imported_lens_and_preserves_explicit_position() {
    let mut session = crate::Session::new();
    session.execute("comp.new", serde_json::json!({"width":1920,"height":1080})).unwrap();
    session.execute("layer.newCamera", serde_json::json!({"name":"Camera"})).unwrap();
    let mut camera = session.project.comps().next().unwrap().1.layers[0].clone();
    let mut next_id = session.project.next_id;
    let zoom = DecodedTransform { path: "cameraOptions/zoom".into(), stream: DecodedStream { value: Some(Value::Scalar(1078.0)), ..Default::default() } };
    install_transforms(std::slice::from_ref(&zoom), &mut camera, &mut Ids(&mut next_id)).unwrap();
    assert_eq!(camera.props.prop("transform/position").unwrap().value, Value::Vec3([960.0, 540.0, -1078.0]));
    let position =
        DecodedTransform { path: "position".into(), stream: DecodedStream { value: Some(Value::Vec3([900.0, 500.0, -854.0])), ..Default::default() } };
    install_transforms(&[zoom, position], &mut camera, &mut Ids(&mut next_id)).unwrap();
    assert_eq!(camera.props.prop("transform/position").unwrap().value, Value::Vec3([900.0, 500.0, -854.0]));
}
