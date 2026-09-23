# PYREN — reproducciones y progreso de correcciones

Última actualización: 2026-09-23

Este documento conserva el resultado de la fase de reproducción y se actualizará a medida que se corrijan los hallazgos. Solo se utilizan los hallazgos que sobrevivieron a la revisión adversarial. S1 quedó fuera por no estar confirmado.

## Estado general

| ID | reproducción | tipo | estado del fix | observaciones |
|---|---|---|---|---|
| C1 | Determinista | Unitario con transición real de `fan.safety` | **Corregido** | Los listeners se invocan desde un snapshot sin retener el mutex del bus; la publicación reentrante termina. |
| C2 | Determinista | Unitario con canales | **Corregido** | Cada pendiente tiene generación y las escrituras GPU se serializan en un gate separado del estado. |
| C3 | Determinista | Integración Linux/procesos | **Corregido** | Todos los comandos externos del daemon se construyen con una máscara limpia de SIGINT/SIGTERM en el hijo. |
| C4 | Determinista | Integración AF_UNIX/procesos | **Corregido** | Un flock persistente por endpoint impide retirar el socket de una instancia viva. |
| C5 | Determinista | Comando externo falso | **Corregido** | Un unload fallido u omitido deja el resultado global fallido aunque el load termine bien. |
| C6 | No conseguida | Requiere evdev/uinput real | No reproducido | El entorno no ofrece `/dev/input` ni `/dev/uinput`; no se creó un test artificial con archivos ordinarios. |
| C7 | Determinista | Unitario | **Corregido** | Guardado de configuración futura protegido. |
| C8 | Determinista | Unitario | **Corregido** | El lookup conserva el dispositivo y el lifecycle revalida `enabled`, distingue `starting` y hace handoff seguro. |
| C9 | Determinista | Backend de hardware falso | **Corregido** | El modo solo se compromete tras un control correcto; un fallo queda pendiente y reintentable sin visibilidad provisional. |
| C10 | Determinista | Backend `tc` falso y canales | **Corregido** | Generaciones requested/committed reconcilian el qdisc y el rollback invalida todo sample antiguo. |
| C11 | Determinista | Unitario | **Corregido** | El baseline y el intervalo de CPU de procesos se conservan entre muestras rápidas. |
| C12 | Determinista | Integración con proceso y FIFO | **Corregido** | El ejecutor común limita `nvidia-smi` a tres segundos, mata su grupo y recoge al hijo. |
| P1 | Determinista | Backend de energía falso y canales | **Corregido** | Snapshot, aplicación y rollback del backend forman una transacción serializada con snapshot fresco. |
| P2 | Determinista | Opener de input inyectado | **Corregido** | `watch` vuelve a comprobar el hardware tras un `NoDevices` inicial. |

## Reproducciones creadas

### Reproducción determinista conseguida

C1, C2, C5, C7, C8, C9, C10, C11, P1 y P2.

### Reproducción conseguida mediante integración

C3, C4 y C12.

### No reproducido convincentemente

C6. Probarlo con un archivo ordinario no demostraría la aceptación real de `EVIOCGRAB` ni la pérdida de eventos, por lo que se dejó sin test.

## Correcciones completadas en este grupo

### C7 — downgrade destructivo de configuración futura

- **Causa raíz:** `ConfigStore::load` detectaba una versión futura y devolvía defaults, pero `ConfigStore::save` no volvía a validar la versión del archivo de destino. Un caller podía guardar esos defaults sobre el archivo que acababa de ser rechazado.
- **Cambio:** `save` lee el destino existente y, si su `version` supera `CURRENT_VERSION`, devuelve `ConfigError::FutureVersion` antes de crear o renombrar el temporal.
- **Por qué elimina la ruta:** la secuencia reproducida `load(TooNew) -> save(defaults)` se detiene antes de toda escritura.
- **Invariante garantizada:** un archivo futuro detectado al comenzar `save` no se reemplaza por un payload de esta versión.
- **Efectos secundarios:** cada guardado sobre un archivo existente realiza una lectura adicional pequeña. Los archivos inexistentes, actuales, antiguos o no parseables conservan el comportamiento previo. No cambia la API pública.
- **Reproducción:** `tests::defaults_loaded_from_a_future_file_cannot_overwrite_that_file` pasa después del fix.
- **Suite:** `pyren-config`: 11 tests y doc-tests, todos correctos.

### C11 — baseline destruido por muestras sin procesos

- **Causa raíz:** `sample(false)` representaba la omisión del paseo de `/proc` mediante una lista vacía, pero la pasaba a `sample_processes`, que reemplazaba `process_ticks` por un mapa vacío. Además, el delta siguiente se habría dividido por el intervalo del tick rápido, aunque los ticks de proceso abarcaban desde la última muestra completa.
- **Cambio:** las muestras sin estadísticas de proceso ya no consumen ni sustituyen el baseline. Se añadió `last_process_sampled`, independiente de `last_sampled`, y las muestras completas calculan CPU con el intervalo transcurrido desde la última muestra completa.
- **Por qué elimina la ruta:** una muestra rápida devuelve una tabla de procesos vacía sin modificar `process_ticks` ni el reloj del muestreo de procesos.
- **Invariantes garantizadas:** omitir procesos no altera su baseline; el numerador y el intervalo usados en el siguiente delta cubren el mismo periodo.
- **Efectos secundarios:** si un barrido completo devuelve inesperadamente cero procesos, se conserva el último baseline para el próximo barrido válido. No cambia el JSON ni la API pública.
- **Reproducción:** `metrics::tests::a_processless_sample_preserves_the_cpu_delta_baseline` pasa después del fix.
- **Suite:** `pyren-system` completa: 31 tests pasan y falla únicamente la reproducción roja pendiente `gpu::tests::a_stuck_nvidia_smi_cannot_stall_a_metrics_sample_forever` (C12). Excluyendo solo C12, 31 tests y doc-tests pasan.

### P2 — ausencia de teclado fijada para toda la vida del módulo

- **Causa raíz:** el constructor guardaba `NoDevices` y un booleano `present` calculado una sola vez. `watch` devolvía `false` usando ese resultado sin volver a consultar los dispositivos, por lo que el worker y su rescan nunca podían arrancar.
- **Cambio:** cuando el único bloqueo almacenado es `NoDevices`, `watch` vuelve a ejecutar el opener antes de decidir. Si encuentra dispositivos, actualiza `devices`, limpia `unavailable` y marca el watcher como activo bajo el mismo lock. `NeedsRoot` conserva el comportamiento anterior. `is_supported` se deriva del estado actual en vez de un booleano inmutable.
- **Por qué elimina la ruta:** un teclado que aparece entre el sondeo inicial y `watch` es detectado, permite arrancar el worker y queda reflejado en capacidades.
- **Invariantes garantizadas:** `watch` no acepta sin verificar el `NoDevices` del constructor; después de un segundo sondeo exitoso, el estado de soporte coincide con el estado que permite iniciar el watcher.
- **Efectos secundarios:** solo el camino inicialmente marcado `NoDevices` realiza un sondeo adicional. No cambia la API pública ni el tratamiento de errores de permisos.
- **Reproducción:** `tests::a_keyboard_appearing_after_the_initial_probe_can_start_the_watcher` pasa después del fix.
- **Suite:** `pyren-hotkey`: 29 tests y doc-tests, todos correctos.

## Validación del grupo C7, C11 y P2

- Los tres tests de reproducción pasan individualmente sin modificar sus assertions.
- `cargo test -p pyren-config`: 11/11 correctos.
- `cargo test -p pyren-hotkey`: 29/29 correctos.
- `cargo test -p pyren-system`: 31 correctos y solo C12 falla deliberadamente.
- `cargo test -p pyren-system -- --skip gpu::tests::a_stuck_nvidia_smi_cannot_stall_a_metrics_sample_forever`: 31/31 correctos.
- `cargo test --no-run -p pyren-config -p pyren-system -p pyren-hotkey`: correcto.
- `cargo clippy -p pyren-config -p pyren-system -p pyren-hotkey`: correcto sin warnings.
- `cargo clippy ... --all-targets`: correcto, con dos warnings `type_complexity` limitados al opener de test introducido para reproducir P2.
- `git diff --check`: correcto.

No se corrigió ningún otro hallazgo en este grupo.

## Correcciones completadas en el grupo C1, C2, C8, C9, C10 y P1

### C1 — deadlock al publicar desde un listener del bus

- **Causa raíz:** `EventBus::publish` mantenía el mutex de `listeners` mientras ejecutaba callbacks. El listener de `power.mode` entra en fan y una transición térmica real publica `fan.safety`; la segunda publicación intentaba adquirir el mismo mutex no reentrante.
- **Cambio:** los listeners se almacenan en `Arc`, `publish` clona el vector bajo el mutex y libera la guarda antes de invocarlos.
- **Invariante:** ningún callback se ejecuta reteniendo un lock del bus. El orden de registro del snapshot y la secuencia del ring se conservan; una suscripción concurrente empieza a recibir en la publicación siguiente.
- **Reproducción:** `fan::tests::a_power_profile_event_can_publish_a_real_fan_safety_transition` pasa y completa dentro del timeout.

### C2 — un revert antiguo desarma un overclock nuevo

- **Causa raíz:** el watchdog trabajaba con un clone de `Pending` y, tras el I/O GPU, limpiaba `pending` sin comprobar que siguiera siendo la misma solicitud. Confirmar la antigua y aplicar otra durante esa ventana permitía que el revert viejo borrase la nueva.
- **Cambio:** cada pendiente lleva una generación monotónica. Apply, reset y revert serializan exclusivamente las escrituras externas mediante un gate GPU separado; el mutex de estado no se mantiene durante el I/O. `reverting` evita confirmar mientras el write-back está en curso y `completed_revert` evita repetirlo en la ventana previa al commit lógico. El resultado solo modifica estado si la generación aún coincide.
- **Invariantes:** una generación solo puede desarmarse a sí misma; nunca se ejecutan dos escrituras GPU simultáneas; `getState` sigue disponible durante el I/O externo; un segundo watchdog no repite una reversión ya completada.
- **Reproducción:** `overclock::tests::an_old_revert_cannot_disarm_a_newer_pending_change` pasa.

### C8 — colisión entre dispositivos y lifecycle perdido del keymap

- **Causa raíz:** la tabla viva colapsaba mappings solo por keycode, y `start` podía devolver al ver `running=true` dejando activo un `stop` ya observado por el worker. Además, una decisión antigua de reiniciar podía competir con un `disable` posterior.
- **Cambio:** la tabla conserva `KeySpec` completo y resuelve primero `(device, keycode)`, usando el mapping sin dispositivo solo como fallback. El lifecycle distingue `starting` de `running`; start y stop vuelven a comprobar `config.enabled` bajo `State` antes de cambiar el flag atómico. Si el worker ya cruzó la salida, hace handoff tras soltar dispositivos y uinput; el nuevo `start` revalida de nuevo antes de cancelar stop o crear un hilo.
- **Invariantes:** dos teclados pueden mapear el mismo keycode de forma independiente; como máximo hay un worker arrancando; la última intención guardada en `enabled` gana sobre side effects antiguos; un worker que ya sale deja un reemplazo solo si sigue habilitado.
- **Reproducciones:** pasan `live_table_keeps_device_scoped_mappings_independent`, `enabling_while_the_old_worker_is_stopping_cancels_its_stop_request`, `a_stale_start_cannot_cancel_stop_after_disable_wins`, `a_stale_stop_cannot_override_a_later_enable` y `disable_between_teardown_and_handoff_cannot_restart_the_worker`.

### C9 — modo de ventilador comprometido después de fallar el hardware

- **Causa raíz:** `setMode` actualizaba modo, configuración y ownership antes del write y registraba la histéresis incluso al fallar. La primera corrección restauraba después de que `tick` soltara `State`, dejando una ventana en la que `getStatus` podía observar el modo provisional.
- **Cambio:** la intención vive en `pending_mode`. Un gate serializa las transiciones; `tick_with_mode_request` prepara los valores tentativos, entrega la misma guarda de `State` a `tick_locked` y, al volver, hace commit o rollback antes de liberarla. Un fallo conserva la intención para el tick periódico, restaura los campos lógicos y no actualiza la histéresis; un retry correcto persiste y anuncia el modo.
- **Invariantes:** ningún lector observa un modo no aplicado; un error no altera el estado comprometido; solo un write correcto suprime reintentos; los eventos se publican después de liberar locks.
- **Reproducciones:** pasan `a_failed_mode_write_does_not_commit_the_requested_mode`, `a_failed_mode_write_is_retried_on_the_next_control_tick` y `a_failed_mode_is_never_visible_between_the_write_and_rollback`.

### C10 — una terminación antigua separa modo lógico y qdisc

- **Causa raíz:** las llamadas a `tc` corrían sin identidad de solicitud. Un apply antiguo podía terminar después del último y sobrescribir el modo lógico o dejar su efecto externo como el último observable.
- **Cambio:** el estado separa `requested`, `committed` y una generación. Cada caller aplica su generación; si al terminar ya no es actual, reaplica la intención más nueva hasta que una generación permanezca estable durante I/O y commit. Un fallo propietario restaura requested a committed e incrementa otra vez la generación, invalidando incluso a un reconciliador que ya hubiese muestreado la intención fallida. `getStatus.mode` expone solo committed.
- **Invariantes:** cuando cesan las solicitudes, modo lógico y qdisc convergen a la última intención válida; un caller antiguo no hace commit de otra generación; un rollback es una transición identificable. El loop no retiene mutex durante `tc`; bajo cambios ininterrumpidos puede demorarse, pero no bloquea productores y converge al quedar estable una generación.
- **Reproducciones:** pasan `the_last_concurrent_request_owns_both_mode_and_qdisc` y `a_failed_request_invalidates_a_reconciler_that_sampled_its_generation`.

### P1 — rollback con snapshot anterior a otra transacción

- **Causa raíz:** `PowerModule` leía el backend antes de adquirir su lock. Un caller esperando podía conservar Balanced, dejar que otro completase Eco y usar después aquel snapshot obsoleto para su rollback.
- **Cambio:** `backend::apply` serializa snapshot, plan, comandos y rollback mediante un mutex de transacción global, y vuelve a leer el snapshot después de adquirirlo. El snapshot recibido por compatibilidad de API deja de ser la fuente del rollback. Los fixtures que comparten overrides de entorno usan un lock común para seguir siendo deterministas bajo el runner paralelo.
- **Invariante:** un rollback solo restaura el estado observado al iniciar su propia transacción; ninguna transacción anterior puede deshacer un commit posterior.
- **Reproducción:** `backend::tests::a_stale_failed_apply_cannot_roll_back_a_later_successful_request` pasa dentro de la suite paralela completa.

## Revisión de concurrencia del grupo

- No aparece ninguna inversión nueva. C2 toma el gate GPU antes de los locks breves de estado/probe; no retiene `State` durante comandos del driver. C10 libera `mode` antes de cada `tc`. P1 mantiene deliberadamente el gate de transacción durante el backend externo, sin adquirir desde ese backend locks del módulo.
- C9 conserva el diseño previo del control loop, que ya hacía sysfs bajo `State`; la transición provisional no amplía esa sección de hardware y ahora termina commit/rollback antes de soltarla. Persistencia y anuncios mantienen el orden: persist dentro del estado comprometido, publicación después de todos los locks.
- C8 tiene wakeup por polling con límite de 500 ms. `SeqCst`, `starting` y el handoff evitan workers duplicados o habilitado-sin-worker en las intercalaciones reproducidas. Los caminos de prueba usan timeouts y no dejan procesos o threads esperando indefinidamente.
- C2 conserva el retry único del watchdog por generación; C9 conserva pendientes fallidos para el siguiente tick; C10 invalida samples mediante tokens, y P1 elimina rollback con snapshot obsoleto. No se encontraron pérdidas de wakeup ni retries suprimidos adicionales en el alcance revisado.

## Validación del grupo C1, C2, C8, C9, C10 y P1

- Todas las reproducciones focales anteriores pasan sin relajar assertions.
- `cargo test -p pyren-core` fuera del sandbox: 74 pasan y fallan únicamente las reproducciones pendientes C3 y C4. `cargo test -p pyren-core -- --skip signals::tests::spawned_processes_do_not_inherit_the_daemons_termination_mask --skip socket::tests::a_second_instance_cannot_replace_a_live_socket`: 74/74 y el doc-test existente queda ignorado.
- En sandbox, la primera corrida de core dio además `EPERM` en seis tests AF_UNIX base; la misma suite fuera del sandbox confirmó que eran restricciones del entorno y que todos esos tests pasan.
- `cargo test -p pyren-fan`: 192/192 y doc-tests correctos.
- `cargo test -p pyren-overclock`: 59/59 y doc-tests correctos.
- `cargo test -p pyren-keymap`: 18/18 y doc-tests correctos.
- `cargo test -p pyren-network`: 15/15 y doc-tests correctos.
- `cargo test -p pyren-power`: 100/100 unitarios; 45/45 integraciones, con un test de minutos ignorado por diseño; doc-tests correctos. Una primera corrida reveló interferencia de variables de entorno entre P1 y otro test y quedó corregida con el lock compartido antes de esta corrida final.
- `cargo test --no-run -p pyren-core -p pyren-fan -p pyren-overclock -p pyren-keymap -p pyren-network -p pyren-power`: correcto.
- `cargo clippy -p pyren-core -p pyren-fan -p pyren-overclock -p pyren-keymap -p pyren-network -p pyren-power --all-targets -- -D warnings`: correcto. La primera corrida señaló `type_complexity` en el hook de test de C10; se extrajo el alias y la corrida final queda limpia.
- `git diff --check`: correcto.

No se corrigieron C3, C4, C5, C6, C12 ni otros hallazgos en este grupo.


## Correcciones C4, C3, C12 y C5 (2026-09-23)

### C4 — segunda instancia sustituye el endpoint vivo

- **Causa raíz:** `bind_restricted` retiraba incondicionalmente la ruta del socket antes de `bind`; el kernel mantiene válido el listener antiguo aunque otro proceso enlace la misma ruta.
- **Cambio:** un archivo de lock persistente junto al endpoint se abre sin seguir symlinks ni bloquear en ficheros especiales; se exige un archivo regular del EUID del daemon y se bloquea con `flock(LOCK_EX | LOCK_NB)` antes de retirar el socket. El descriptor vive con el listener. Un socket obsoleto se puede retirar tras adquirir el lock.
- **Invariante:** dos instancias cooperativas que usan la misma ruta y el mismo archivo de lock no pueden publicar el endpoint a la vez mientras la primera conserva su listener.
- **Efectos secundarios:** queda un archivo `.sock.lock` persistente; una segunda instancia recibe `AddrInUse` en lugar de desplazar a la primera. No cambia la API pública; el lock se libera por cierre del descriptor incluso en shutdown abrupto.
- **Reproducción:** `socket::tests::a_second_instance_cannot_replace_a_live_socket`, sin cambiar assertions, pasa fuera del sandbox; AF_UNIX devuelve `EPERM` dentro.
- **Suite:** `pyren-core` completa, 77 tests y doc-test ignorado, correcta tras C3 y el test adicional del supervisor. Durante C4 solo fallaba la reproducción roja de C3.
- **Supuesto:** el directorio y el archivo de lock no son retirados o sustituidos por un tercero mientras vive el daemon. La instancia competidora ejecuta el mismo protocolo de lock; una versión antigua sin este fix no lo respetaría.

### C3 — máscara de terminación heredada por hijos

- **Causa raíz:** la máscara de señales es por hilo y se hereda en fork/exec; `std::process::Command` no la limpiaba. La documentación del módulo asumía incorrectamente lo contrario.
- **Cambio:** `pyren_core::process::command` instala un hook `pre_exec` que desbloquea únicamente SIGINT y SIGTERM en el hijo. Todas las rutas de comandos externos de los crates del daemon, incluidas las que usaban `output`, `status` y el supervisor común, construyen ahora el comando por esa función. El hilo padre conserva la máscara que necesita `sigwait`.
- **Invariante:** los hijos creados por las rutas de producción del daemon no heredan el bloqueo interno de SIGINT/SIGTERM.
- **Efectos secundarios:** las rutas pasan por `fork` al usar `pre_exec`; no cambia la firma pública de las operaciones ni el tratamiento de shutdown del padre. Los hijos responden a señales de terminación de forma normal.
- **Reproducción:** `signals::tests::spawned_processes_do_not_inherit_the_daemons_termination_mask`, con la misma assertion sobre `SigBlk`, pasa.
- **Suite:** `pyren-core` completa, 77 tests correctos. `clippy` de los crates con comandos externos afectadas por esta migración pasa con `--all-targets -- -D warnings`.
- **Supuesto:** nuevos sitios que creen `std::process::Command` directamente deberán usar la abstracción común; los binarios independientes de `check` no ejecutan el daemon.

### C12 — métricas NVIDIA sin límite temporal

- **Causa raíz:** `read_nvidia_gpus_from` usaba `Command::output`, que espera indefinidamente. El sampler espera a todos sus trabajos antes de publicar una muestra.
- **Cambio:** `nvidia-smi` usa `pyren_core::process::output`, con plazo de tres segundos. El supervisor crea un grupo de procesos para cada comando con plazo, observa tanto la salida del hijo como el cierre de las tuberías y, al vencer el plazo, mata el grupo y recoge al hijo mediante `wait`.
- **Invariante:** un `nvidia-smi` que se bloquea, o deja un descendiente ordinario con las tuberías abiertas, no mantiene la muestra más allá del plazo del supervisor y la pequeña latencia de polling/terminación; el hijo directo se recoge.
- **Efectos secundarios:** una consulta NVIDIA lenta devuelve una lista vacía para esa muestra. Los comandos supervisados viven en un grupo de procesos propio. No cambia el JSON ni la API pública.
- **Reproducción:** `gpu::tests::a_stuck_nvidia_smi_cannot_stall_a_metrics_sample_forever` pasa sin relajar assertions; el test focal del supervisor `a_descendant_cannot_hold_the_output_pipe_open_past_the_deadline` cubre tuberías heredadas.
- **Suite:** `pyren-system` completa, 32 tests correctos, y `pyren-core` completa correcta (77 tests).
- **Supuesto:** un descendiente que se independice explícitamente de su grupo y retenga las tuberías puede sobrevivir al kill del grupo; el límite de retorno sigue vigente porque el supervisor no espera indefinidamente a lectores bloqueados.

### C5 — éxito falso tras un unload fallido

- **Causa raíz:** `modprobe-remove` era opcional para permitir el caso sin módulo cargado. `execute_watched` solo contaba fallos de pasos obligatorios, por lo que un fallo de unload seguido por un load exitoso producía `succeeded=true` aunque el módulo antiguo seguía activo.
- **Cambio:** cualquier `modprobe-remove` advertido o declinado marca la recarga como incompleta; la ejecución intenta el `modprobe` posterior y conserva los resultados por paso, pero el informe global devuelve `succeeded=false`.
- **Invariante:** ningún unload fallido u omitido puede quedar oculto por un load posterior al calcular el éxito de la transición solicitada.
- **Efectos secundarios:** el unload fallido por ausencia real del módulo también devuelve fallo global aunque el load funcione; es un falso negativo deliberado frente a un falso éxito. La API y los estados por paso no cambian.
- **Estado parcial y rollback:** si el unload falla, el módulo vivo puede seguir siendo el anterior, aunque archivos de driver o configuración ya estén preparados para el siguiente arranque. Un rollback automático de esos archivos podría alterar el próximo arranque o deshacer trabajo válido; se deja el estado explícito como fallido para inspección o reintento. Si unload y load terminan bien, sus códigos de salida son la verificación disponible de la transición; no se comprobó identidad del módulo en hardware real.
- **Reproducción:** `execute::tests::a_busy_old_module_cannot_be_reported_as_a_successful_reload` pasa sin cambiar assertions.
- **Suite:** `pyren-installer` completa, 93 tests correctos.
- **Supuesto:** se confía en los códigos de salida de `modprobe`; otros actores que carguen un módulo concurrentemente quedan fuera de la transacción del instalador.

### Revisión conjunta

- C4 mantiene el lock durante todo el `serve_unix_socket`; el descriptor usa close-on-exec de `OpenOptions`, por lo que C3/C12 no prolongan accidentalmente su vida en hijos. El archivo no se retira al salir, y el kernel libera el flock en shutdown normal o abrupto.
- C3 modifica la máscara solo después de fork; no desbloquea señales del daemon ni compite con `sigwait`. C12 mata el grupo del comando supervisado y recoge al hijo directo, sin esperar indefinidamente las tuberías. Los comandos normales mantienen sus semánticas de salida.
- C5 no retiene locks durante el comando externo y no crea procesos persistentes. Los resultados de pasos siguen informando el fallo parcial. No se hallaron inversiones nuevas de locks ni cambios incompatibles de API en los cuatro fixes.
- Validación final: `cargo test -p pyren-core` (77), `cargo test -p pyren-system` (32), `cargo test -p pyren-installer` (93), `cargo clippy -p pyren-core -p pyren-system -p pyren-installer -p pyren-overclock -p pyren-network -p pyren-power -p pyren-fan -p pyren-rgb --all-targets -- -D warnings` y `git diff --check` pasan. `cargo test --workspace -- --test-threads=1` pasa completo. Una corrida paralela inicial tuvo dos fallos intermitentes de FIFO en RGB; `cargo test -p pyren-rgb --lib` pasó al repetir, y la corrida serial completa pasó. `cargo clippy --workspace --all-targets -- -D warnings` sigue fallando por dos avisos `type_complexity` previos de los tests de hotkey, fuera del alcance de estos cuatro fixes.
- C6 queda sin tocar.
